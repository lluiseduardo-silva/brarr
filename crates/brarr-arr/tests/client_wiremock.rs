//! Wire-level tests for [`brarr_arr::ArrClient`] against a wiremock
//! mock of a Sonarr / Radarr v3 instance.
//!
//! Pins the parts of the contract that matter:
//! - `X-Api-Key` header is sent on every request
//! - `system/status` returns parsed [`SystemStatus`] on 200
//! - `release/push` POSTs a camelCase body with the expected keys
//! - non-2xx responses surface as [`ArrError::Http`] with the body
//!   captured for the operator to debug

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use brarr_arr::{ArrClient, ArrError, ArrInstance, ArrKind, ArrProtocol, PushReleasePayload};
use time::OffsetDateTime;
use url::Url;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const API_KEY: &str = "test-arr-key-1234";

fn client(server: &MockServer, kind: ArrKind) -> ArrClient {
    let inst = ArrInstance {
        name: "test".into(),
        kind,
        base_url: Url::parse(&server.uri()).unwrap(),
        api_key: API_KEY.into(),
    };
    ArrClient::new(inst).unwrap()
}

fn sample_push() -> PushReleasePayload {
    PushReleasePayload {
        title: "The.Matrix.1999.1080p.BluRay-FOO".into(),
        download_url: "https://brarr.local/torznab/download/abc".into(),
        protocol: ArrProtocol::Torrent,
        publish_date: OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
        size: 9_608_016_733,
        indexer: "brarr".into(),
        info_url: Some("https://capybara/torrents/1".into()),
        seeders: Some(42),
        leechers: Some(1),
    }
}

#[tokio::test]
async fn ping_returns_system_status_on_200() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/system/status"))
        .and(header("X-Api-Key", API_KEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "appName": "Radarr",
            "version": "5.0.0.1234",
            "buildTime": "2024-01-01T00:00:00Z"
        })))
        .mount(&server)
        .await;

    let c = client(&server, ArrKind::Radarr);
    let st = c.ping().await.unwrap();
    assert_eq!(st.app_name, "Radarr");
    assert_eq!(st.version, "5.0.0.1234");
}

#[tokio::test]
async fn ping_returns_http_error_on_401() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/system/status"))
        .respond_with(ResponseTemplate::new(401).set_body_string("{\"message\":\"bad apikey\"}"))
        .mount(&server)
        .await;

    let c = client(&server, ArrKind::Sonarr);
    let err = c.ping().await.unwrap_err();
    match err {
        ArrError::Http { status, body, .. } => {
            assert_eq!(status, 401);
            assert!(body.contains("bad apikey"), "body = {body}");
        }
        other => panic!("expected Http error, got {other:?}"),
    }
}

#[tokio::test]
async fn push_release_sends_camelcase_body_with_apikey_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v3/release/push"))
        .and(header("X-Api-Key", API_KEY))
        .and(body_partial_json(serde_json::json!({
            "title": "The.Matrix.1999.1080p.BluRay-FOO",
            "downloadUrl": "https://brarr.local/torznab/download/abc",
            "protocol": "torrent",
            "size": 9_608_016_733_u64,
            "indexer": "brarr",
            "infoUrl": "https://capybara/torrents/1",
            "seeders": 42,
            "leechers": 1
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;

    let c = client(&server, ArrKind::Radarr);
    let body = c
        .push_release(&sample_push())
        .await
        .expect("push should succeed");
    assert!(body.contains("[]") || body.is_empty(), "body = {body}");
}

#[tokio::test]
async fn push_release_propagates_http_400_with_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v3/release/push"))
        .respond_with(
            ResponseTemplate::new(400).set_body_string("{\"errors\":[\"Unknown movie\"]}"),
        )
        .mount(&server)
        .await;

    let c = client(&server, ArrKind::Radarr);
    let err = c.push_release(&sample_push()).await.unwrap_err();
    match err {
        ArrError::Http { status, body, .. } => {
            assert_eq!(status, 400);
            assert!(body.contains("Unknown movie"), "body = {body}");
        }
        other => panic!("expected Http error, got {other:?}"),
    }
}

#[tokio::test]
async fn push_release_succeeds_when_arr_responds_with_empty_array() {
    // Sonarr/Radarr return `[]` on a successful push when the release
    // was accepted but no rejections fired.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v3/release/push"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .mount(&server)
        .await;

    let c = client(&server, ArrKind::Sonarr);
    c.push_release(&sample_push()).await.unwrap();
}

#[tokio::test]
async fn monitored_movies_returns_parsed_list() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/movie"))
        .and(header("X-Api-Key", API_KEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {
                "id": 1,
                "title": "The Matrix",
                "tmdbId": 603,
                "imdbId": "tt0133093",
                "monitored": true,
                "hasFile": false
            },
            {
                "id": 2,
                "title": "Inception",
                "tmdbId": 27205,
                "imdbId": "tt1375666",
                "monitored": true,
                "hasFile": true
            }
        ])))
        .mount(&server)
        .await;

    let c = client(&server, ArrKind::Radarr);
    let movies = c.monitored_movies().await.unwrap();
    assert_eq!(movies.len(), 2);
    assert_eq!(movies[0].title, "The Matrix");
    assert_eq!(movies[0].tmdb_id, 603);
    assert_eq!(movies[0].imdb_id, "tt0133093");
    assert!(movies[0].monitored);
    assert!(!movies[0].has_file);
    assert!(movies[1].has_file);
}

#[tokio::test]
async fn monitored_movies_propagates_http_404() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/movie"))
        .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
        .mount(&server)
        .await;

    let c = client(&server, ArrKind::Sonarr);
    let err = c.monitored_movies().await.unwrap_err();
    match err {
        ArrError::Http { status, .. } => assert_eq!(status, 404),
        other => panic!("expected Http 404, got {other:?}"),
    }
}

#[tokio::test]
async fn push_release_supports_usenet_protocol_value() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v3/release/push"))
        .and(body_partial_json(
            serde_json::json!({ "protocol": "usenet" }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .mount(&server)
        .await;

    let c = client(&server, ArrKind::Radarr);
    let mut payload = sample_push();
    payload.protocol = ArrProtocol::Usenet;
    c.push_release(&payload).await.unwrap();
}
