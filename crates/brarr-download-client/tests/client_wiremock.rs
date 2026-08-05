//! Wire-level tests for the two download clients.
//!
//! Both programs have an authentication scheme that a status-code-only
//! client would misread, so these pin the exact shapes:
//!
//! - qBittorrent answers `Ok.` / `Fails.` with `200` either way, keeps a
//!   `SID` cookie, and returns `403` once that cookie expires.
//! - SABnzbd answers `{"status": false, "error": …}` with `200` when the
//!   apikey is wrong.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown
)]

use brarr_download_client::{
    DownloadClient, DownloadClientConfig, DownloadClientError, DownloadClientKind, Protocol,
    QbittorrentClient, SabnzbdClient, build,
};
use url::Url;
use wiremock::matchers::{body_string_contains, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SID: &str = "8ndkPPS3D+ZQ0dGvbTOl";
const SAB_KEY: &str = "sab-api-key-1234";

fn qb_config(server: &MockServer, username: Option<&str>) -> DownloadClientConfig {
    DownloadClientConfig {
        name: "qbittorrent-main".to_owned(),
        kind: DownloadClientKind::Qbittorrent,
        base_url: Url::parse(&server.uri()).unwrap(),
        username: username.map(str::to_owned),
        password: username.map(|_| "hunter2".to_owned()),
        api_key: None,
        category: Some("brarr".to_owned()),
    }
}

fn sab_config(server: &MockServer, key: &str) -> DownloadClientConfig {
    DownloadClientConfig {
        name: "sabnzbd-main".to_owned(),
        kind: DownloadClientKind::Sabnzbd,
        base_url: Url::parse(&server.uri()).unwrap(),
        username: None,
        password: None,
        api_key: Some(key.to_owned()),
        category: Some("brarr".to_owned()),
    }
}

async fn mount_qb_login(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/api/v2/auth/login"))
        .and(body_string_contains("username=admin"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("set-cookie", format!("SID={SID}; HttpOnly; path=/"))
                .set_body_string("Ok."),
        )
        .mount(server)
        .await;
}

#[tokio::test]
async fn qbittorrent_logs_in_and_reports_the_version() {
    let server = MockServer::start().await;
    mount_qb_login(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/v2/app/version"))
        .respond_with(ResponseTemplate::new(200).set_body_string("v5.0.4"))
        .mount(&server)
        .await;

    let client = QbittorrentClient::new(qb_config(&server, Some("admin"))).unwrap();
    let status = client.test_connection().await.unwrap();
    assert_eq!(status.version, "v5.0.4");

    let hits = server.received_requests().await.unwrap();
    assert_eq!(
        hits.iter()
            .filter(|r| r.url.path() == "/api/v2/app/version")
            .count(),
        1
    );
    // The session cookie has to travel on the follow-up call, otherwise
    // qBittorrent would answer 403 for every request after the login.
    let version_req = hits
        .iter()
        .find(|r| r.url.path() == "/api/v2/app/version")
        .unwrap();
    let cookie = version_req.headers.get("cookie").unwrap().to_str().unwrap();
    assert_eq!(cookie, format!("SID={SID}"));
}

#[tokio::test]
async fn qbittorrent_reports_bad_credentials_as_auth_not_success() {
    let server = MockServer::start().await;
    // The trap: `Fails.` arrives with HTTP 200.
    Mock::given(method("POST"))
        .and(path("/api/v2/auth/login"))
        .respond_with(ResponseTemplate::new(200).set_body_string("Fails."))
        .mount(&server)
        .await;

    let client = QbittorrentClient::new(qb_config(&server, Some("admin"))).unwrap();
    let err = client.test_connection().await.unwrap_err();
    match err {
        DownloadClientError::Auth { detail, .. } => assert_eq!(detail, "Fails."),
        other => panic!("expected an auth failure, got {other:?}"),
    }
}

#[tokio::test]
async fn qbittorrent_ip_ban_is_explained_rather_than_shown_as_403() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v2/auth/login"))
        .respond_with(ResponseTemplate::new(403).set_body_string("Your IP address has been banned"))
        .mount(&server)
        .await;

    let client = QbittorrentClient::new(qb_config(&server, Some("admin"))).unwrap();
    let err = client.test_connection().await.unwrap_err();
    match err {
        DownloadClientError::Auth { detail, .. } => {
            assert!(detail.contains("banido"), "got {detail}");
        }
        other => panic!("expected an auth failure, got {other:?}"),
    }
}

#[tokio::test]
async fn qbittorrent_without_a_username_skips_the_login_entirely() {
    let server = MockServer::start().await;
    // No login mock is mounted: a request to it would 404 and fail the
    // test, which is exactly the assertion — a WebUI with authentication
    // bypassed must not be logged into.
    Mock::given(method("GET"))
        .and(path("/api/v2/app/version"))
        .respond_with(ResponseTemplate::new(200).set_body_string("v4.6.7"))
        .mount(&server)
        .await;

    let client = QbittorrentClient::new(qb_config(&server, None)).unwrap();
    assert_eq!(client.test_connection().await.unwrap().version, "v4.6.7");

    let hits = server.received_requests().await.unwrap();
    assert!(
        hits.iter().all(|r| r.url.path() != "/api/v2/auth/login"),
        "no login should have been attempted"
    );
}

#[tokio::test]
async fn qbittorrent_reauthenticates_once_when_the_session_expired() {
    let server = MockServer::start().await;
    mount_qb_login(&server).await;
    // First call answers 403 (qBittorrent restarted, SID dropped), then
    // the endpoint behaves. Higher priority so it wins while it lasts.
    Mock::given(method("GET"))
        .and(path("/api/v2/app/version"))
        .respond_with(ResponseTemplate::new(403).set_body_string("Forbidden"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v2/app/version"))
        .respond_with(ResponseTemplate::new(200).set_body_string("v5.0.4"))
        .with_priority(2)
        .mount(&server)
        .await;

    let client = QbittorrentClient::new(qb_config(&server, Some("admin"))).unwrap();
    assert_eq!(client.test_connection().await.unwrap().version, "v5.0.4");

    let hits = server.received_requests().await.unwrap();
    assert_eq!(
        hits.iter()
            .filter(|r| r.url.path() == "/api/v2/auth/login")
            .count(),
        2,
        "one login up front, one after the 403"
    );
}

#[tokio::test]
async fn qbittorrent_keeps_the_session_across_calls() {
    let server = MockServer::start().await;
    mount_qb_login(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/v2/app/version"))
        .respond_with(ResponseTemplate::new(200).set_body_string("v5.0.4"))
        .mount(&server)
        .await;

    let client = QbittorrentClient::new(qb_config(&server, Some("admin"))).unwrap();
    client.test_connection().await.unwrap();
    client.test_connection().await.unwrap();

    let hits = server.received_requests().await.unwrap();
    assert_eq!(
        hits.iter()
            .filter(|r| r.url.path() == "/api/v2/auth/login")
            .count(),
        1,
        "the SID is reused; a login per call would be a needless round-trip"
    );
}

#[tokio::test]
async fn sabnzbd_reads_the_version_out_of_the_queue_payload() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "queue"))
        .and(query_param("apikey", SAB_KEY))
        .and(query_param("output", "json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "queue": {
                "status": "Downloading",
                "paused": false,
                "version": "4.3.2",
                "diskspace1": "2100.00",
                "slots": []
            }
        })))
        .mount(&server)
        .await;

    let client = SabnzbdClient::new(sab_config(&server, SAB_KEY)).unwrap();
    let status = client.test_connection().await.unwrap();
    assert_eq!(status.version, "4.3.2");
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "the queue payload carries the version — no second call needed"
    );
}

#[tokio::test]
async fn sabnzbd_falls_back_to_mode_version_on_older_builds() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "queue"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "queue": { "status": "Idle", "paused": false, "slots": [] }
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "version"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "version": "3.7.2" })),
        )
        .mount(&server)
        .await;

    let client = SabnzbdClient::new(sab_config(&server, SAB_KEY)).unwrap();
    assert_eq!(client.test_connection().await.unwrap().version, "3.7.2");
}

#[tokio::test]
async fn sabnzbd_treats_a_200_refusal_as_an_auth_failure() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "status": false, "error": "API Key Incorrect" }),
            ),
        )
        .mount(&server)
        .await;

    let client = SabnzbdClient::new(sab_config(&server, "wrong")).unwrap();
    let err = client.test_connection().await.unwrap_err();
    match err {
        DownloadClientError::Auth { detail, .. } => assert_eq!(detail, "API Key Incorrect"),
        other => panic!("expected an auth failure, got {other:?}"),
    }
}

#[tokio::test]
async fn sabnzbd_server_side_errors_are_not_labelled_credentials() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            serde_json::json!({ "status": false, "error": "Incomplete directory unreachable" }),
        ))
        .mount(&server)
        .await;

    let client = SabnzbdClient::new(sab_config(&server, SAB_KEY)).unwrap();
    let err = client.test_connection().await.unwrap_err();
    assert!(
        matches!(err, DownloadClientError::Http { .. }),
        "got {err:?}"
    );
}

#[tokio::test]
async fn a_dead_host_surfaces_as_transport_not_as_a_bad_password() {
    // Port 1 is reserved and unbindable by an unprivileged process, so
    // the connection is refused rather than answered.
    let config = DownloadClientConfig {
        name: "sabnzbd-desligado".to_owned(),
        kind: DownloadClientKind::Sabnzbd,
        base_url: Url::parse("http://127.0.0.1:1/").unwrap(),
        username: None,
        password: None,
        api_key: Some(SAB_KEY.to_owned()),
        category: None,
    };
    let client = SabnzbdClient::new(config).unwrap();
    let err = client.test_connection().await.unwrap_err();
    assert!(
        matches!(err, DownloadClientError::Transport { .. }),
        "got {err:?}"
    );
}

#[tokio::test]
async fn the_factory_dispatches_on_kind() {
    let server = MockServer::start().await;
    let qb = build(qb_config(&server, Some("admin"))).unwrap();
    assert_eq!(qb.kind(), DownloadClientKind::Qbittorrent);
    assert_eq!(qb.kind().protocol(), Protocol::Torrent);
    assert_eq!(qb.name(), "qbittorrent-main");

    let sab = build(sab_config(&server, SAB_KEY)).unwrap();
    assert_eq!(sab.kind(), DownloadClientKind::Sabnzbd);
    assert_eq!(sab.kind().protocol(), Protocol::Usenet);

    // A SABnzbd row with no key never reaches the network.
    let mut broken = sab_config(&server, SAB_KEY);
    broken.api_key = None;
    match build(broken) {
        Err(DownloadClientError::Config { .. }) => {}
        Err(other) => panic!("expected a config error, got {other:?}"),
        Ok(_) => panic!("a keyless SABnzbd row must not build"),
    }
}
