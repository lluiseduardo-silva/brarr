//! HTTP-level tests for [`TvdbClient`] against a mock `TheTVDB`.
//!
//! Two kinds of payload, deliberately:
//!
//! - **Hand-built JSON**, for the shapes: the `camelCase`/`snake_case`
//!   split between records and `links`, the season-type path segment,
//!   the pagination cursor, the re-login, the retry policy. Pagination in
//!   particular can only be built by hand — `page_size` is 500 upstream,
//!   so no series brarr cares about is more than one real page.
//! - **Responses captured from the live v4 API**, for the *numbers* —
//!   see `tests/fixtures/README.md`. Those numbers are the reason this
//!   crate exists, and until now they were asserted only by
//!   `tests/live_api.rs`, which is `#[ignore]`d behind a key and a
//!   network. A claim only a skipped test defends is a claim nobody
//!   checks.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on happy paths"
)]

use std::time::Duration;

use brarr_tvdb::{RetryConfig, SeasonType, TvdbAuth, TvdbClient, TvdbError};
use serde_json::json;
use wiremock::matchers::{body_string_contains, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn auth() -> TvdbAuth {
    TvdbAuth {
        api_key: "test-project-key".to_owned(),
        pin: None,
    }
}

/// Retry off by default, so a test that asserts on a failure does not
/// sleep through the backoff — the same contract `brarr-tmdb`'s wiremock
/// tests use. The three tests about the policy itself opt back in.
fn client(server: &MockServer) -> TvdbClient {
    TvdbClient::with_base_url(auth(), &format!("{}/v4", server.uri()))
        .unwrap()
        .with_retry(RetryConfig::disabled())
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

/// **A hung connection has to end.**
///
/// The builder configured only a `user_agent`, so a socket that accepts
/// and never answers held the call forever. Tolerable while this crate
/// was a 12-hour best-effort sweep; not once it is the source of the
/// episode tree and sits in the path of `/library/add`, where the wait
/// is an operator staring at a request that never returns.
#[tokio::test]
async fn a_hung_connection_times_out() {
    let server = MockServer::start().await;
    mock_login(&server).await;
    Mock::given(method("GET"))
        .and(path("/v4/series/1/episodes/official"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(30))
                .set_body_json(json!({ "data": { "episodes": [] } })),
        )
        .mount(&server)
        .await;

    let client = TvdbClient::with_base_url(auth(), &format!("{}/v4", server.uri()))
        .unwrap()
        .with_retry(RetryConfig::disabled())
        .with_timeout(Duration::from_millis(300));

    let started = std::time::Instant::now();
    let failed = client.series_episodes(1, SeasonType::Official, None).await;

    match failed {
        Err(TvdbError::Http(e)) => assert!(e.is_timeout(), "wrong failure: {e}"),
        other => panic!("a hung connection must time out, got {other:?}"),
    }
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the timeout did not fire"
    );
}

/// A 5xx is the failure worth another attempt: it heals on its own,
/// which is the entire difference from a refused key.
#[tokio::test]
async fn a_transient_5xx_is_retried_then_succeeds() {
    let server = MockServer::start().await;
    mock_login(&server).await;
    // wiremock serves mounts in order and honours `up_to_n_times`, so the
    // first attempt gets the 502 and the second the real page.
    Mock::given(method("GET"))
        .and(path("/v4/series/1/episodes/official"))
        .respond_with(ResponseTemplate::new(502))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v4/series/1/episodes/official"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "series": { "id": 1, "name": "S" }, "episodes": [episode(9, 1, 1, Some(1))] }
        })))
        .mount(&server)
        .await;

    let client = TvdbClient::with_base_url(auth(), &format!("{}/v4", server.uri()))
        .unwrap()
        .with_retry(RetryConfig {
            max_attempts: 3,
            base_delay: Duration::from_millis(0),
        });

    let found = client
        .series_episodes(1, SeasonType::Official, None)
        .await
        .unwrap();
    assert_eq!(found.episodes.len(), 1);
}

/// **A refused key must not be multiplied.** It will not heal, and
/// hammering someone else's free tier three times per title over 180
/// titles is how a polite client stops being one.
#[tokio::test]
async fn a_refused_key_is_not_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v4/login"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;

    let client = TvdbClient::with_base_url(auth(), &format!("{}/v4", server.uri()))
        .unwrap()
        .with_retry(RetryConfig {
            max_attempts: 5,
            base_delay: Duration::from_millis(0),
        });

    assert!(matches!(
        client.verify().await,
        Err(TvdbError::Unauthorized)
    ));
    // `expect(1)` above is asserted when the server drops.
}

// ---------------------------------------------------------------------
// Against responses captured from the live API. See fixtures/README.md.
// ---------------------------------------------------------------------

/// Serve one captured page and walk it through the real client.
async fn from_fixture(
    name: &str,
    series: i64,
    season_type: SeasonType,
) -> Vec<brarr_tvdb::Episode> {
    let body = std::fs::read_to_string(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap_or_else(|e| panic!("fixture {name}: {e}"));

    let server = MockServer::start().await;
    mock_login(&server).await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/v4/series/{series}/episodes/{}",
            season_type.as_str()
        )))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(body, "application/json")
                .append_header("content-type", "application/json"),
        )
        .mount(&server)
        .await;

    client(&server)
        .series_episodes(series, season_type, None)
        .await
        .unwrap()
        .episodes
}

/// Episodes per season, specials excluded, in season order.
fn shape(episodes: &[brarr_tvdb::Episode]) -> Vec<usize> {
    let mut by_season: std::collections::BTreeMap<i32, usize> = std::collections::BTreeMap::new();
    for e in episodes.iter().filter(|e| e.season_number > 0) {
        *by_season.entry(e.season_number).or_default() += 1;
    }
    by_season.into_values().collect()
}

/// **The number this crate exists for, in the default suite.**
///
/// TMDB models Dragon Ball Super as one season of 131. The disk, Sonarr
/// and every release call it five seasons of 14/13/19/30/55 — and so does
/// `TheTVDB`'s `official` season type.
///
/// The capture also carries two specials in season 0, so the exclusion is
/// exercised on real data rather than assumed: 133 records in, 131
/// counted.
#[tokio::test]
async fn dragon_ball_super_official_is_14_13_19_30_55() {
    let episodes = from_fixture("dbs_official_page0.json", 295_068, SeasonType::Official).await;

    assert_eq!(episodes.len(), 133, "specials are parsed, not dropped");
    assert_eq!(shape(&episodes), vec![14, 13, 19, 30, 55]);

    // The absolute number is what joins this back to TMDB's flat season:
    // arc 2 episode 1 is the fifteenth of the series. **Advisory, never a
    // join key** — a special sitting on the absolute axis is exactly how
    // Kaiju No. 8 shifted a whole season by one.
    let arc2e1 = episodes
        .iter()
        .find(|e| e.season_number == 2 && e.number == 1)
        .expect("S02E01 exists");
    assert_eq!(arc2e1.absolute_number, Some(15));
    assert!(arc2e1.aired.is_some(), "the air date is what arbitrates");
}

/// One season of 25 on TMDB, two of 12 and 13 here, and every release
/// follows this one. TMDB is not wrong — whoever publishes cuts
/// somewhere else.
#[tokio::test]
async fn solo_leveling_official_is_12_13() {
    let episodes = from_fixture(
        "solo_leveling_official_page0.json",
        389_597,
        SeasonType::Official,
    )
    .await;
    assert_eq!(shape(&episodes), vec![12, 13]);
}

/// The absolute axis is one run of numbers straight through — the same
/// 131 episodes, no season 0 at all, which is why a special can shift the
/// official axis without appearing on this one.
#[tokio::test]
async fn the_absolute_axis_is_one_run_of_131() {
    let episodes = from_fixture("dbs_absolute_page0.json", 295_068, SeasonType::Absolute).await;
    assert_eq!(shape(&episodes), vec![131]);
    assert!(
        episodes.iter().all(|e| e.season_number > 0),
        "the absolute axis carries no specials"
    );
}

/// One episode with an explicit name, so a test can say "untranslated"
/// by passing `None` — which is exactly what the API sends.
fn named(id: i64, season: i64, number: i64, name: Option<&str>) -> serde_json::Value {
    json!({
        "id": id,
        "name": name,
        "seasonNumber": season,
        "number": number,
        "absoluteNumber": null,
        "aired": "2023-09-29",
    })
}

fn page(episodes: &[serde_json::Value]) -> serde_json::Value {
    json!({
        "status": "success",
        "data": {
            "series": { "id": 424_536, "name": "Sōsō no Frieren" },
            "episodes": episodes,
        },
        "links": { "next": null, "total_items": episodes.len(), "page_size": 500 }
    })
}

/// **The names brarr stored were the original language, because it never
/// asked for one.** Measured live on 2026-08-14: Frieren's episodes come
/// back as `冒険の終わり`, Portuguese has 0 of 66 translated and English
/// 65 of 66. Doctor Who has 154 of 322 in Portuguese — so the fallback is
/// **per episode**, never per series.
///
/// The join key is `TheTVDB`'s episode id, which is stable across season
/// types and therefore across languages too.
#[tokio::test]
async fn an_untranslated_episode_falls_back_through_the_chain() {
    let server = MockServer::start().await;
    mock_login(&server).await;

    // Portuguese: one of the three, the way Doctor Who looks.
    Mock::given(method("GET"))
        .and(path("/v4/series/424536/episodes/official/por"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(&[
            named(1, 1, 1, Some("O Fim da Jornada")),
            named(2, 1, 2, None),
            named(3, 1, 3, None),
        ])))
        .mount(&server)
        .await;
    // English fills the second; the third is the one only the original
    // has, which is Frieren's shape.
    Mock::given(method("GET"))
        .and(path("/v4/series/424536/episodes/official/eng"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(&[
            named(1, 1, 1, Some("The Journey's End")),
            named(2, 1, 2, Some("It Didn't Have to Be Magic")),
            named(3, 1, 3, None),
        ])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v4/series/424536/episodes/official"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(&[
            named(1, 1, 1, Some("冒険の終わり")),
            named(2, 1, 2, Some("別に魔法じゃなくたって…")),
            named(3, 1, 3, Some("人を殺す魔法")),
        ])))
        .mount(&server)
        .await;

    let found = client(&server)
        .series_episodes_in(424_536, SeasonType::Official, None, &["por", "eng"])
        .await
        .unwrap();

    let name = |id: i64| {
        found
            .episodes
            .iter()
            .find(|e| e.id == id)
            .and_then(|e| e.name.clone())
    };
    assert_eq!(
        name(1).as_deref(),
        Some("O Fim da Jornada"),
        "português vence"
    );
    assert_eq!(
        name(2).as_deref(),
        Some("It Didn't Have to Be Magic"),
        "inglês preenche o que o português não tem"
    );
    assert_eq!(
        name(3).as_deref(),
        Some("人を殺す魔法"),
        "e o original é o último recurso, nunca o primeiro"
    );
    assert_eq!(found.episodes.len(), 3, "o conjunto não muda com o idioma");
}

/// A series fully translated costs **one** request, not three. The walk
/// is skipped the moment nothing is left to fill — over 180 series that
/// is the difference between a refresh and a rate limit.
#[tokio::test]
async fn a_fully_translated_series_asks_once() {
    let server = MockServer::start().await;
    mock_login(&server).await;
    Mock::given(method("GET"))
        .and(path("/v4/series/424536/episodes/official/por"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(&[
            named(1, 1, 1, Some("O Fim da Jornada")),
            named(2, 1, 2, Some("Não Precisava Ser Magia")),
        ])))
        .expect(1)
        .mount(&server)
        .await;
    // Mounted with `expect(0)`: reaching English would mean the walk
    // does not stop when the gap closes.
    Mock::given(method("GET"))
        .and(path("/v4/series/424536/episodes/official/eng"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(&[])))
        .expect(0)
        .mount(&server)
        .await;

    let found = client(&server)
        .series_episodes_in(424_536, SeasonType::Official, None, &["por", "eng"])
        .await
        .unwrap();
    assert_eq!(found.episodes.len(), 2);
}
