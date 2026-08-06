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
async fn wanted_episodes_single_page_returns_parsed_list() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/wanted/missing"))
        .and(header("X-Api-Key", API_KEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "page": 1,
            "pageSize": 200,
            "totalRecords": 2,
            "records": [
                {
                    "id": 1,
                    "title": "Pilot",
                    "seasonNumber": 1,
                    "episodeNumber": 1,
                    "monitored": true,
                    "hasFile": false,
                    "series": {
                        "id": 10,
                        "title": "Some Show",
                        "tvdbId": 12345,
                        "monitored": true
                    }
                },
                {
                    "id": 2,
                    "title": "Two",
                    "seasonNumber": 1,
                    "episodeNumber": 2,
                    "monitored": true,
                    "hasFile": false,
                    "series": {
                        "id": 10,
                        "title": "Some Show",
                        "tvdbId": 12345,
                        "monitored": true
                    }
                }
            ]
        })))
        .mount(&server)
        .await;
    let c = client(&server, ArrKind::Sonarr);
    let eps = c.wanted_episodes().await.unwrap();
    assert_eq!(eps.len(), 2);
    assert_eq!(eps[0].series.as_ref().unwrap().tvdb_id, 12345);
    assert_eq!(eps[0].season_number, 1);
    assert_eq!(eps[0].episode_number, 1);
}

#[tokio::test]
async fn wanted_episodes_backfills_series_from_separate_endpoint() {
    // Sonarr v4 sometimes returns `/wanted/missing` rows with no
    // nested `series` projection even when `includeSeries=true` is
    // passed. The client must follow up with `/api/v3/series` and
    // join by series_id to recover the tvdb id.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/wanted/missing"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "page": 1, "pageSize": 200, "totalRecords": 1,
            "records": [
                {
                    "id": 1, "seriesId": 42, "title": "Pilot",
                    "seasonNumber": 1, "episodeNumber": 1,
                    "monitored": true, "hasFile": false
                }
            ]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v3/series"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            { "id": 42, "title": "Joined Show", "tvdbId": 999, "monitored": true }
        ])))
        .mount(&server)
        .await;
    let c = client(&server, ArrKind::Sonarr);
    let eps = c.wanted_episodes().await.unwrap();
    assert_eq!(eps.len(), 1);
    let s = eps[0].series.as_ref().expect("backfilled");
    assert_eq!(s.tvdb_id, 999);
    assert_eq!(s.title, "Joined Show");
}

#[tokio::test]
async fn wanted_episodes_404_surfaces_as_http_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/wanted/missing"))
        .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
        .mount(&server)
        .await;
    let c = client(&server, ArrKind::Sonarr);
    let err = c.wanted_episodes().await.unwrap_err();
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

// ---------------------------------------------------------------------
// Catalogue reads — the migration path.
//
// The payloads below mirror what the operator's own stack answers,
// captured live: Sonarr v3 does carry `tmdbId` on the series (so no
// TVDB bridge is needed), and every path comes back in the *arr's own
// namespace — `/data/Series/…` against brarr's `/midias/Series`. The
// tests pin that the client hands the raw path through untranslated,
// because translating is the caller's decision and doing it here would
// bury it.
// ---------------------------------------------------------------------

#[tokio::test]
async fn catalogue_series_carries_paths_seasons_and_the_tmdb_id() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/series"))
        .and(header("X-Api-Key", API_KEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {
                "id": 12,
                "title": "9-1-1",
                "year": 2018,
                "tvdbId": 328_724,
                "tmdbId": 75_219,
                "imdbId": "tt7908628",
                "monitored": true,
                "path": "/data/Series/9-1-1",
                "rootFolderPath": "/data/Series",
                "qualityProfileId": 4,
                "seasons": [
                    { "seasonNumber": 0, "monitored": false },
                    { "seasonNumber": 1, "monitored": true },
                    { "seasonNumber": 2, "monitored": false }
                ]
            }
        ])))
        .mount(&server)
        .await;

    let c = client(&server, ArrKind::Sonarr);
    let series = c.catalogue_series().await.unwrap();
    assert_eq!(series.len(), 1);
    let s = &series[0];
    assert_eq!(s.title, "9-1-1");
    assert_eq!(s.year, 2018);
    assert_eq!(s.tmdb_id, 75_219, "Sonarr v3 carries the TMDb id itself");
    assert_eq!(s.tvdb_id, 328_724);
    assert_eq!(
        s.path, "/data/Series/9-1-1",
        "the path stays in the *arr's namespace — translating is the caller's call"
    );
    assert_eq!(s.root_folder_path, "/data/Series");
    assert_eq!(s.seasons.len(), 3);
    assert!(!s.seasons[0].monitored, "season 0 is the specials bucket");
    assert!(s.seasons[1].monitored);
    assert!(!s.seasons[2].monitored);
}

/// A Sonarr that predates `tmdbId` on the series object leaves it at
/// zero rather than failing to parse — the caller then bridges through
/// TMDB's find-by-TVDB endpoint.
#[tokio::test]
async fn a_series_without_a_tmdb_id_still_parses() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/series"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            { "id": 1, "title": "Bofuri", "tvdbId": 368_128 }
        ])))
        .mount(&server)
        .await;

    let c = client(&server, ArrKind::Sonarr);
    let series = c.catalogue_series().await.unwrap();
    assert_eq!(series[0].tmdb_id, 0, "absent means unlinked, not an error");
    assert_eq!(series[0].tvdb_id, 368_128);
    assert!(series[0].seasons.is_empty());
    assert!(series[0].path.is_empty());
}

/// The episode list is fetched *without* `includeEpisodeFile`, so it
/// carries the file id and not the file. This is what keeps one series
/// at a few KB instead of the ~300 KB the inlined form costs.
#[tokio::test]
async fn episodes_carry_the_file_id_and_files_resolve_it() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/episode"))
        .and(header("X-Api-Key", API_KEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {
                "id": 100,
                "seasonNumber": 1,
                "episodeNumber": 1,
                "monitored": true,
                "hasFile": true,
                "episodeFileId": 900
            },
            {
                "id": 101,
                "seasonNumber": 1,
                "episodeNumber": 2,
                "monitored": true,
                "hasFile": false,
                "episodeFileId": 0
            }
        ])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v3/episodefile"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {
                "id": 900,
                "path": "/data/Series/9-1-1/Season 1/9-1-1.S01E01.1080p.DUAL-Eri.mkv",
                "size": 2_147_483_648_u64
            }
        ])))
        .mount(&server)
        .await;

    let c = client(&server, ArrKind::Sonarr);
    let eps = c.episodes(12).await.unwrap();
    assert_eq!(eps.len(), 2);
    assert_eq!(eps[0].episode_file_id, 900);
    assert!(eps[0].has_file);
    assert_eq!(
        eps[1].episode_file_id, 0,
        "no file is zero, which is how Sonarr spells absent"
    );

    let files = c.episode_files(12).await.unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].id, 900);
    assert!(files[0].path.ends_with("S01E01.1080p.DUAL-Eri.mkv"));
}

#[tokio::test]
async fn catalogue_movies_carries_the_file_path() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/movie"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {
                "id": 7,
                "title": "Scary Movie",
                "year": 2000,
                "tmdbId": 4_247,
                "imdbId": "tt0175142",
                "monitored": true,
                "hasFile": true,
                "path": "/data/Filmes/Scary Movie (2000)",
                "rootFolderPath": "/data/Filmes",
                "movieFile": {
                    "id": 55,
                    "path": "/data/Filmes/Scary Movie (2000)/Scary.Movie.2000.1080p.mkv",
                    "size": 9_000_000_000_u64
                }
            },
            {
                "id": 8,
                "title": "Duna",
                "year": 2021,
                "tmdbId": 438_631,
                "monitored": true,
                "hasFile": false,
                "path": "/data/Filmes/Duna (2021)",
                "rootFolderPath": "/data/Filmes"
            }
        ])))
        .mount(&server)
        .await;

    let c = client(&server, ArrKind::Radarr);
    let movies = c.catalogue_movies().await.unwrap();
    assert_eq!(movies.len(), 2);
    let file = movies[0].movie_file.as_ref().expect("has a file");
    assert!(file.path.ends_with("Scary.Movie.2000.1080p.mkv"));
    assert!(
        movies[1].movie_file.is_none(),
        "a monitored movie with no file is the migration's whole point: \
         it enters the catalogue and brarr starts looking for it"
    );
}

#[tokio::test]
async fn root_folders_are_read_in_the_arrs_own_namespace() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/rootfolder"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {
                "id": 1,
                "path": "/data/Series",
                "accessible": true,
                "unmappedFolders": [ { "name": "Scorpion", "path": "/data/Series/Scorpion" } ]
            }
        ])))
        .mount(&server)
        .await;

    let c = client(&server, ArrKind::Sonarr);
    let roots = c.root_folders().await.unwrap();
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].path, "/data/Series");
}
