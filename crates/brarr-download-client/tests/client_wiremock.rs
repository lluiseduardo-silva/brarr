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
    DownloadClient, DownloadClientConfig, DownloadClientError, DownloadClientKind, DownloadState,
    Protocol, QbittorrentClient, ReleaseFile, SabnzbdClient, build,
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

/// A structurally valid torrent, so the client can take its infohash.
/// SHA-1 of its `info` dictionary is [`TORRENT_HASH`].
const TORRENT: &[u8] = b"d8:announce11:https://t/a4:infod6:lengthi1024e4:name10:Matrix.mkv12:piece lengthi16384e6:pieces0:e13:creation datei1700000000ee";
const TORRENT_HASH: &str = "659d65ffe26eab1ba01deb5a4d3daeb91d46e715";
const NZB: &[u8] =
    br#"<?xml version="1.0"?><nzb xmlns="http://www.newzbin.com/DTD/2003/nzb"></nzb>"#;

#[tokio::test]
async fn qbittorrent_uploads_the_torrent_with_its_category() {
    let server = MockServer::start().await;
    mount_qb_login(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/add"))
        .respond_with(ResponseTemplate::new(200).set_body_string("Ok."))
        .mount(&server)
        .await;

    let client = QbittorrentClient::new(qb_config(&server, Some("admin"))).unwrap();
    let added = client
        .add("Matrix.1999.1080p.BluRay", ReleaseFile::Bytes(TORRENT))
        .await
        .unwrap();
    // qBittorrent answers `Ok.` and names nothing, so the identity has
    // to come from the file — the same infohash the client keys on.
    assert_eq!(added.client_item_id.as_deref(), Some(TORRENT_HASH));

    let hits = server.received_requests().await.unwrap();
    let add = hits
        .iter()
        .find(|r| r.url.path() == "/api/v2/torrents/add")
        .unwrap();
    let body = String::from_utf8_lossy(&add.body);
    assert!(body.contains("name=\"torrents\""), "sent as a file part");
    assert!(body.contains("Matrix.1999.1080p.BluRay.torrent"));
    assert!(body.contains("8:announce"), "the torrent bytes travel");
    assert!(body.contains("brarr"), "the configured category is applied");
    let cookie = add.headers.get("cookie").unwrap().to_str().unwrap();
    assert_eq!(cookie, format!("SID={SID}"));
}

#[tokio::test]
async fn qbittorrent_sends_a_magnet_as_a_url_not_a_file() {
    let server = MockServer::start().await;
    mount_qb_login(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/add"))
        .respond_with(ResponseTemplate::new(200).set_body_string("Ok."))
        .mount(&server)
        .await;

    let client = QbittorrentClient::new(qb_config(&server, Some("admin"))).unwrap();
    client
        .add("Matrix", ReleaseFile::Magnet("magnet:?xt=urn:btih:abc"))
        .await
        .unwrap();

    let hits = server.received_requests().await.unwrap();
    let add = hits
        .iter()
        .find(|r| r.url.path() == "/api/v2/torrents/add")
        .unwrap();
    let body = String::from_utf8_lossy(&add.body);
    assert!(body.starts_with("urls="), "form-encoded, got {body}");
    assert!(body.contains("magnet"));
}

#[tokio::test]
async fn qbittorrent_refusing_a_torrent_in_a_200_is_still_a_failure() {
    let server = MockServer::start().await;
    mount_qb_login(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/add"))
        // The same trap as the login, on the path that matters most:
        // reporting this as success would mark the grab sent forever.
        .respond_with(ResponseTemplate::new(200).set_body_string("Fails."))
        .mount(&server)
        .await;

    let client = QbittorrentClient::new(qb_config(&server, Some("admin"))).unwrap();
    let err = client
        .add("Matrix", ReleaseFile::Bytes(TORRENT))
        .await
        .unwrap_err();
    match err {
        DownloadClientError::Http { body, .. } => assert_eq!(body, "Fails."),
        other => panic!("expected the refusal to surface, got {other:?}"),
    }
}

#[tokio::test]
async fn sabnzbd_uploads_the_nzb_and_keeps_the_nzo_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(query_param("mode", "addfile"))
        .and(query_param("apikey", SAB_KEY))
        .and(query_param("cat", "brarr"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": true,
            "nzo_ids": ["SABnzbd_nzo_p86tgx"]
        })))
        .mount(&server)
        .await;

    let client = SabnzbdClient::new(sab_config(&server, SAB_KEY)).unwrap();
    let added = client
        .add("Matrix.1999.1080p.WEB-DL", ReleaseFile::Bytes(NZB))
        .await
        .unwrap();
    assert_eq!(
        added.client_item_id.as_deref(),
        Some("SABnzbd_nzo_p86tgx"),
        "the handle exists only at hand-off time — losing it means losing the download"
    );

    let hits = server.received_requests().await.unwrap();
    let body = String::from_utf8_lossy(&hits[0].body);
    assert!(body.contains("name=\"name\""), "the upload field is `name`");
    assert!(body.contains("Matrix.1999.1080p.WEB-DL.nzb"));
    assert!(body.contains("newzbin"), "the nzb bytes travel");
}

#[tokio::test]
async fn sabnzbd_queuing_nothing_is_not_a_success() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "status": true, "nzo_ids": [] })),
        )
        .mount(&server)
        .await;

    let client = SabnzbdClient::new(sab_config(&server, SAB_KEY)).unwrap();
    let err = client
        .add("Matrix", ReleaseFile::Bytes(NZB))
        .await
        .unwrap_err();
    assert!(
        matches!(err, DownloadClientError::Http { .. }),
        "got {err:?}"
    );
}

#[tokio::test]
async fn sabnzbd_refuses_a_magnet_before_touching_the_network() {
    let server = MockServer::start().await;
    let client = SabnzbdClient::new(sab_config(&server, SAB_KEY)).unwrap();
    let err = client
        .add("Matrix", ReleaseFile::Magnet("magnet:?xt=urn:btih:abc"))
        .await
        .unwrap_err();
    assert!(matches!(err, DownloadClientError::Config { .. }));
    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "no request should have been made"
    );
}

#[tokio::test]
async fn qbittorrent_magnet_identity_comes_from_the_link() {
    let server = MockServer::start().await;
    mount_qb_login(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/v2/torrents/add"))
        .respond_with(ResponseTemplate::new(200).set_body_string("Ok."))
        .mount(&server)
        .await;

    let client = QbittorrentClient::new(qb_config(&server, Some("admin"))).unwrap();
    let added = client
        .add(
            "Matrix",
            ReleaseFile::Magnet(&format!(
                "magnet:?xt=urn:btih:{}",
                TORRENT_HASH.to_uppercase()
            )),
        )
        .await
        .unwrap();
    assert_eq!(added.client_item_id.as_deref(), Some(TORRENT_HASH));
}

#[tokio::test]
async fn qbittorrent_status_reports_progress_and_maps_the_state() {
    let server = MockServer::start().await;
    mount_qb_login(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/v2/torrents/info"))
        .and(query_param("hashes", TORRENT_HASH))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!([{
                "hash": TORRENT_HASH,
                "name": "Matrix.1999.1080p",
                "state": "downloading",
                "progress": 0.62,
                "size": 4_400_000_000i64,
                "dlspeed": 8_200_000i64,
                "eta": 540,
                "content_path": "/downloads/Matrix.1999.1080p",
                "category": "brarr"
            }])),
        )
        .mount(&server)
        .await;

    let client = QbittorrentClient::new(qb_config(&server, Some("admin"))).unwrap();
    let status = client.status(TORRENT_HASH).await.unwrap().unwrap();
    assert_eq!(status.state, DownloadState::Downloading);
    assert!((status.progress - 0.62).abs() < 0.001);
    assert_eq!(status.size_bytes, Some(4_400_000_000));
    assert_eq!(status.speed_bytes, Some(8_200_000));
    assert_eq!(status.eta_seconds, Some(540));
    assert_eq!(
        status.save_path.as_deref(),
        Some("/downloads/Matrix.1999.1080p")
    );
}

#[tokio::test]
async fn qbittorrent_seeding_counts_as_finished() {
    let server = MockServer::start().await;
    mount_qb_login(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/v2/torrents/info"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!([{
                "hash": TORRENT_HASH,
                // Past the download, into seeding — which never ends on its
                // own, so waiting for it would mean waiting forever.
                "state": "stalledUP",
                "progress": 1.0,
                "size": 10i64,
                "dlspeed": 0i64,
                // qBittorrent's "no estimate" sentinel: 100 days.
                "eta": 8_640_000i64,
                "content_path": ""
            }])),
        )
        .mount(&server)
        .await;

    let client = QbittorrentClient::new(qb_config(&server, Some("admin"))).unwrap();
    let status = client.status(TORRENT_HASH).await.unwrap().unwrap();
    assert_eq!(status.state, DownloadState::Completed);
    assert_eq!(status.speed_bytes, None, "zero is absent, not zero");
    assert_eq!(status.eta_seconds, None, "the sentinel is not an estimate");
    assert_eq!(status.save_path, None);
}

#[tokio::test]
async fn qbittorrent_status_is_none_for_a_torrent_it_does_not_have() {
    let server = MockServer::start().await;
    mount_qb_login(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/v2/torrents/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;

    let client = QbittorrentClient::new(qb_config(&server, Some("admin"))).unwrap();
    assert!(client.status(TORRENT_HASH).await.unwrap().is_none());
}

#[tokio::test]
async fn sabnzbd_status_finds_a_running_job_in_the_queue() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "queue"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "queue": {
                "status": "Downloading",
                "slots": [
                    { "nzo_id": "SABnzbd_nzo_other", "status": "Queued", "percentage": "0",
                      "mb": "100.0", "timeleft": "0:00:00" },
                    { "nzo_id": "SABnzbd_nzo_p86tgx", "status": "Downloading", "percentage": "94",
                      "mb": "2867.2", "timeleft": "0:00:12" }
                ]
            }
        })))
        .mount(&server)
        .await;

    let client = SabnzbdClient::new(sab_config(&server, SAB_KEY)).unwrap();
    let status = client.status("SABnzbd_nzo_p86tgx").await.unwrap().unwrap();
    assert_eq!(status.state, DownloadState::Downloading);
    assert!((status.progress - 0.94).abs() < 0.001);
    assert_eq!(status.size_bytes, Some(2867 * 1024 * 1024));
    assert_eq!(status.eta_seconds, Some(12));
    assert_eq!(
        status.speed_bytes, None,
        "SABnzbd reports speed per queue, never per job"
    );
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "found in the queue — no need to read the history"
    );
}

#[tokio::test]
async fn sabnzbd_status_falls_through_to_the_history_once_finished() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "queue"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "queue": { "slots": [] } })),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "history"))
        .and(query_param("nzo_ids", "SABnzbd_nzo_p86tgx"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "history": { "slots": [{
                "nzo_id": "SABnzbd_nzo_p86tgx",
                "name": "Matrix.1999.1080p",
                "status": "Completed",
                "storage": "/downloads/complete/Matrix.1999.1080p",
                "fail_message": ""
            }]}
        })))
        .mount(&server)
        .await;

    let client = SabnzbdClient::new(sab_config(&server, SAB_KEY)).unwrap();
    let status = client.status("SABnzbd_nzo_p86tgx").await.unwrap().unwrap();
    assert_eq!(status.state, DownloadState::Completed);
    assert!((status.progress - 1.0).abs() < f32::EPSILON);
    assert_eq!(
        status.save_path.as_deref(),
        Some("/downloads/complete/Matrix.1999.1080p")
    );
    assert_eq!(status.detail, None);
}

#[tokio::test]
async fn sabnzbd_a_failed_job_carries_its_reason() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "queue"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "queue": { "slots": [] } })),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "history"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "history": { "slots": [{
                "nzo_id": "SABnzbd_nzo_p86tgx",
                "status": "Failed",
                "storage": "",
                "fail_message": "Unpacking failed, archive requires a password"
            }]}
        })))
        .mount(&server)
        .await;

    let client = SabnzbdClient::new(sab_config(&server, SAB_KEY)).unwrap();
    let status = client.status("SABnzbd_nzo_p86tgx").await.unwrap().unwrap();
    assert_eq!(status.state, DownloadState::Failed);
    assert_eq!(
        status.detail.as_deref(),
        Some("Unpacking failed, archive requires a password")
    );
}

#[tokio::test]
async fn sabnzbd_status_is_none_when_neither_queue_nor_history_has_it() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "queue"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "queue": { "slots": [] } })),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("mode", "history"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "history": { "slots": [] } })),
        )
        .mount(&server)
        .await;

    let client = SabnzbdClient::new(sab_config(&server, SAB_KEY)).unwrap();
    assert!(client.status("SABnzbd_nzo_gone").await.unwrap().is_none());
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
