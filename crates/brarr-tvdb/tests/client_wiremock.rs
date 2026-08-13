//! HTTP-level tests for [`TvdbClient`] against a mock `TheTVDB`.
//!
//! Fixtures are derived from the documented v4 schema rather than
//! captured live — see `tests/fixtures/README.md`. The shapes that
//! matter are pinned here: the `camelCase`/`snake_case` split between
//! records and `links`, the season-type path segment, and the
//! pagination cursor.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on happy paths"
)]

use brarr_tvdb::{SeasonType, TvdbAuth, TvdbClient, TvdbError};
use serde_json::json;
use wiremock::matchers::{body_string_contains, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn auth() -> TvdbAuth {
    TvdbAuth {
        api_key: "test-project-key".to_owned(),
        pin: None,
    }
}

fn client(server: &MockServer) -> TvdbClient {
    TvdbClient::with_base_url(auth(), &format!("{}/v4", server.uri())).unwrap()
}

async fn mock_login(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v4/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "success",
            "data": { "token": "a-month-long-token" }
        })))
        .mount(server)
        .await;
}

/// One episode as the v4 schema spells it — `camelCase` records.
fn episode(id: i64, season: i64, number: i64, absolute: Option<i64>) -> serde_json::Value {
    json!({
        "id": id,
        "name": format!("Episódio {number}"),
        "seasonNumber": season,
        "number": number,
        "absoluteNumber": absolute,
        "aired": "2016-01-17",
    })
}

/// **The reason this crate exists.** `TheTVDB`'s official season type is
/// the split releases use: Dragon Ball Super's arc 2 episode 1 is the
/// fifteenth of the series, and both coordinates come back together.
#[tokio::test]
async fn official_episodes_carry_the_split_releases_use() {
    let server = MockServer::start().await;
    mock_login(&server).await;
    Mock::given(method("GET"))
        .and(path("/v4/series/295068/episodes/official"))
        .and(header("authorization", "Bearer a-month-long-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "success",
            "data": {
                "series": { "id": 295_068, "name": "Dragon Ball Super" },
                "episodes": [episode(1, 2, 1, Some(15)), episode(2, 2, 2, Some(16))],
            },
            "links": { "next": null, "total_items": 2, "page_size": 500 }
        })))
        .mount(&server)
        .await;

    let found = client(&server)
        .series_episodes(295_068, SeasonType::Official, None)
        .await
        .unwrap();

    assert_eq!(found.series_name.as_deref(), Some("Dragon Ball Super"));
    assert_eq!(found.episodes.len(), 2);
    assert_eq!(found.episodes[0].season_number, 2);
    assert_eq!(found.episodes[0].number, 1);
    assert_eq!(
        found.episodes[0].absolute_number,
        Some(15),
        "the absolute axis is what joins this back to TMDB's flat season"
    );
}

/// `links` is `snake_case` while records are `camelCase` — an inconsistency
/// in the API, and a blanket `rename_all` would null the cursor and stop
/// the walk after one page.
#[tokio::test]
async fn pagination_follows_the_cursor_to_the_end() {
    let server = MockServer::start().await;
    mock_login(&server).await;
    Mock::given(method("GET"))
        .and(path("/v4/series/1/episodes/absolute"))
        .and(query_param("page", "0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "series": { "id": 1, "name": "Longa" }, "episodes": [episode(1, 1, 1, Some(1))] },
            "links": { "next": "https://api4.thetvdb.com/v4/series/1/episodes/absolute?page=1" }
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v4/series/1/episodes/absolute"))
        .and(query_param("page", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "series": { "id": 1, "name": "Longa" }, "episodes": [episode(2, 1, 2, Some(2))] },
            "links": { "next": null }
        })))
        .mount(&server)
        .await;

    let found = client(&server)
        .series_episodes(1, SeasonType::Absolute, None)
        .await
        .unwrap();
    assert_eq!(found.episodes.len(), 2, "both pages were collected");
}

/// A `next` that repeats itself must end the walk, not spin doing real
/// requests until the page ceiling.
#[tokio::test]
async fn a_cursor_that_repeats_itself_terminates() {
    let server = MockServer::start().await;
    mock_login(&server).await;
    Mock::given(method("GET"))
        .and(path("/v4/series/7/episodes/default"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "series": { "id": 7 }, "episodes": [episode(1, 1, 1, None)] },
            "links": { "next": "https://api4.thetvdb.com/v4/series/7/episodes/default?page=0" }
        })))
        .expect(2)
        .mount(&server)
        .await;

    let found = client(&server)
        .series_episodes(7, SeasonType::Default, None)
        .await
        .unwrap();
    assert_eq!(
        found.episodes.len(),
        1,
        "the repeat added nothing and stopped"
    );
}

/// One login per client, not one per call. The token lasts a month.
#[tokio::test]
async fn the_token_is_fetched_once_and_reused() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v4/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "token": "a-month-long-token" }
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v4/series/1/episodes/official"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "series": { "id": 1 }, "episodes": [] },
            "links": { "next": null }
        })))
        .mount(&server)
        .await;

    let client = client(&server);
    client
        .series_episodes(1, SeasonType::Official, None)
        .await
        .unwrap();
    client
        .series_episodes(1, SeasonType::Official, None)
        .await
        .unwrap();
    // `expect(1)` on the login mock is the assertion; it verifies on drop.
}

/// A project key funded by the revenue tier must not send `pin` at all —
/// the documentation says to remove it, and an empty string is not the
/// same as absent to this API.
#[tokio::test]
async fn a_project_key_sends_no_pin() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v4/login"))
        .and(body_string_contains("apikey"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "token": "t" }
        })))
        .mount(&server)
        .await;

    client(&server).verify().await.unwrap();
    let body = &server.received_requests().await.unwrap()[0].body;
    let sent: serde_json::Value = serde_json::from_slice(body).unwrap();
    assert!(sent.get("pin").is_none(), "sent {sent}");
}

/// A user-supported key carries the subscriber PIN. brarr's is not one,
/// but supporting it is a field rather than a code change.
#[tokio::test]
async fn a_user_supported_key_sends_the_pin() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v4/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "token": "t" }
        })))
        .mount(&server)
        .await;

    let client = TvdbClient::with_base_url(
        TvdbAuth {
            api_key: "k".to_owned(),
            pin: Some("ABCD1234".to_owned()),
        },
        &format!("{}/v4", server.uri()),
    )
    .unwrap();
    client.verify().await.unwrap();

    let body = &server.received_requests().await.unwrap()[0].body;
    let sent: serde_json::Value = serde_json::from_slice(body).unwrap();
    assert_eq!(sent["pin"], "ABCD1234");
}

/// A refused key is its own variant, because the message has to name the
/// PIN — the two credential shapes fail identically otherwise.
#[tokio::test]
async fn a_refused_key_says_what_to_check() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v4/login"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let err = client(&server).verify().await.unwrap_err();
    assert!(matches!(err, TvdbError::Unauthorized), "{err:?}");
    assert!(err.to_string().contains("PIN"), "{err}");
}

/// A 200 with no token is a refusal too. Status alone would report a
/// broken key as a healthy connection — the same trap
/// `brarr-download-client` documents for qBittorrent's `Fails.`.
#[tokio::test]
async fn a_login_with_no_token_is_a_refusal() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v4/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": {} })))
        .mount(&server)
        .await;

    assert!(matches!(
        client(&server).verify().await.unwrap_err(),
        TvdbError::Unauthorized
    ));
}

/// A month-long token outlives most processes but not all of them. One
/// re-login, then the call succeeds — not a loop.
#[tokio::test]
async fn a_rejected_token_triggers_exactly_one_relogin() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v4/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "token": "fresh" }
        })))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v4/series/9/episodes/official"))
        .and(header("authorization", "Bearer fresh"))
        .respond_with(ResponseTemplate::new(401))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v4/series/9/episodes/official"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "series": { "id": 9 }, "episodes": [episode(1, 1, 1, None)] },
            "links": { "next": null }
        })))
        .mount(&server)
        .await;

    let found = client(&server)
        .series_episodes(9, SeasonType::Official, None)
        .await
        .unwrap();
    assert_eq!(found.episodes.len(), 1);
}

#[tokio::test]
async fn an_unknown_series_is_not_found() {
    let server = MockServer::start().await;
    mock_login(&server).await;
    Mock::given(method("GET"))
        .and(path("/v4/series/404/episodes/official"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    assert!(matches!(
        client(&server)
            .series_episodes(404, SeasonType::Official, None)
            .await
            .unwrap_err(),
        TvdbError::NotFound(_)
    ));
}

/// A base URL without a trailing slash makes `Url::join` drop the last
/// segment, turning `/v4/` into `/` and every call into a 404.
#[tokio::test]
async fn a_base_url_without_a_trailing_slash_still_works() {
    let server = MockServer::start().await;
    mock_login(&server).await;
    Mock::given(method("GET"))
        .and(path("/v4/series/1/episodes/official"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "series": { "id": 1 }, "episodes": [] },
            "links": { "next": null }
        })))
        .mount(&server)
        .await;

    let client = TvdbClient::with_base_url(auth(), &format!("{}/v4", server.uri())).unwrap();
    assert!(
        client
            .series_episodes(1, SeasonType::Official, None)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn a_remote_id_resolves_to_a_series() {
    let server = MockServer::start().await;
    mock_login(&server).await;
    Mock::given(method("GET"))
        .and(path("/v4/search/remoteid/tt4644488"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{ "series": { "id": 295_068, "name": "Dragon Ball Super" } }]
        })))
        .mount(&server)
        .await;

    assert_eq!(
        client(&server)
            .series_by_remote_id("tt4644488")
            .await
            .unwrap(),
        Some(295_068)
    );
}
