//! HTTP-level tests for [`TmdbClient`] against a mock TMDB.
//!
//! Fixtures are derived from the documented v3 schema — see
//! `tests/fixtures/README.md` for why they could not be captured live
//! and what each one pins down.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on happy paths"
)]

use brarr_tmdb::{RetryConfig, TmdbClient, TmdbError};
use time::macros::date;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

/// Shaped like a real v4 read access token (a JWT) so the client picks
/// the bearer path. A non-JWT string would be treated as a v3 API key.
const FAKE_V4_TOKEN: &str = "eyJhbGciOiJIUzI1NiJ9.cGF5bG9hZA.c2ln";

/// Shaped like a v3 API key: 32 hex characters, no dots.
const FAKE_V3_KEY: &str = "94d0f0e1a2b3c4d5e6f708192a3b4c5d";

fn client(server: &MockServer) -> TmdbClient {
    TmdbClient::new(FAKE_V4_TOKEN)
        .unwrap()
        .with_base_url(&server.uri())
        .unwrap()
        .with_retry(RetryConfig::disabled())
}

fn json(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body.to_owned(), "application/json")
}

#[tokio::test]
async fn a_v4_token_travels_as_a_bearer_header() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search/movie"))
        .and(header("authorization", &*format!("Bearer {FAKE_V4_TOKEN}")))
        .respond_with(json(&fixture("search_movie_duna.json")))
        .expect(1)
        .mount(&server)
        .await;

    client(&server).search_movies("duna", None).await.unwrap();
}

#[tokio::test]
async fn a_v3_api_key_travels_as_a_query_parameter() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search/movie"))
        .and(query_param("api_key", FAKE_V3_KEY))
        .respond_with(json(&fixture("search_movie_duna.json")))
        .expect(1)
        .mount(&server)
        .await;

    // Verified against the live API: this key sent as a bearer header
    // returns 401, and as `?api_key=` returns 200. Detecting the shape
    // spares the operator having to know which string they pasted.
    TmdbClient::new(FAKE_V3_KEY)
        .unwrap()
        .with_base_url(&server.uri())
        .unwrap()
        .with_retry(RetryConfig::disabled())
        .search_movies("duna", None)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_v3_api_key_sends_no_authorization_header() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search/movie"))
        .and(wiremock::matchers::header_exists("accept"))
        .respond_with(json(&fixture("search_movie_duna.json")))
        .mount(&server)
        .await;

    TmdbClient::new(FAKE_V3_KEY)
        .unwrap()
        .with_base_url(&server.uri())
        .unwrap()
        .with_retry(RetryConfig::disabled())
        .search_movies("duna", None)
        .await
        .unwrap();

    let sent = &server.received_requests().await.unwrap()[0];
    assert!(
        !sent.headers.contains_key("authorization"),
        "a v3 key must not also be offered as a bearer token"
    );
}

#[tokio::test]
async fn a_blank_credential_is_refused_before_any_request() {
    assert!(matches!(
        TmdbClient::new("   "),
        Err(TmdbError::InvalidToken)
    ));
}

#[tokio::test]
async fn search_movies_maps_results_and_survives_null_poster() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search/movie"))
        .and(query_param("query", "duna"))
        .and(query_param("language", "pt-BR"))
        .respond_with(json(&fixture("search_movie_duna.json")))
        .mount(&server)
        .await;

    let hits = client(&server).search_movies("duna", None).await.unwrap();

    // A real page of results: 20 hits, ranked by TMDB's own relevance.
    assert_eq!(hits.len(), 20);
    assert_eq!(hits[0].tmdb_id, 438_631);
    assert_eq!(hits[0].title, "Duna");
    assert_eq!(hits[0].year(), Some(2021));

    // The long tail of a real search is messy, and the parser has to
    // survive all of it rather than only the well-formed head.
    assert_eq!(
        hits.iter().filter(|h| h.poster_path.is_none()).count(),
        3,
        "obscure entries carry no poster"
    );
    assert_eq!(
        hits.iter().filter(|h| h.overview.is_none()).count(),
        14,
        "most results have no pt-BR synopsis — TMDB sends \"\" not null"
    );
}

#[tokio::test]
async fn search_movies_pins_the_year_when_given() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search/movie"))
        .and(query_param("primary_release_year", "2024"))
        .respond_with(json(&fixture("search_movie_duna.json")))
        .expect(1)
        .mount(&server)
        .await;

    client(&server)
        .search_movies("duna", Some(2024))
        .await
        .unwrap();
}

#[tokio::test]
async fn movie_details_fold_in_append_to_response() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/movie/603"))
        .and(query_param(
            "append_to_response",
            "external_ids,release_dates,translations",
        ))
        .respond_with(json(&fixture("movie_603.json")))
        .mount(&server)
        .await;

    let movie = client(&server).movie(603).await.unwrap();

    assert_eq!(movie.tmdb_id, 603);
    assert_eq!(movie.imdb_id.as_deref(), Some("tt0133093"));
    assert_eq!(movie.title, "Matrix");
    assert_eq!(movie.runtime_minutes, Some(136));
    assert_eq!(movie.release_date, Some(date!(1999 - 03 - 31)));
    assert_eq!(movie.status.as_deref(), Some("Released"));
    assert!(
        movie
            .overview
            .as_deref()
            .is_some_and(|o| o.contains("Thomas Anderson")),
        "pt-BR synopsis comes back on the top level for this title"
    );
}

#[tokio::test]
async fn an_old_film_has_a_physical_date_but_no_digital_one() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/movie/603"))
        .respond_with(json(&fixture("movie_603.json")))
        .mount(&server)
        .await;

    let movie = client(&server).movie(603).await.unwrap();

    // BR reports theatrical (type 3) and Blu-ray (type 5) only. Neither
    // BR nor US carries a digital date, and the one that does exist —
    // AE, 2016-01-07 — is deliberately not borrowed: see release_date_of.
    assert_eq!(
        movie.digital_release, None,
        "no digital window in BR or US means we do not know one"
    );
    assert_eq!(movie.physical_release, Some(date!(2008 - 10 - 14)));
}

#[tokio::test]
async fn a_recent_film_resolves_the_brazilian_digital_window() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/movie/693134"))
        .respond_with(json(&fixture("movie_693134.json")))
        .mount(&server)
        .await;

    let movie = client(&server).movie(693_134).await.unwrap();

    assert_eq!(movie.title, "Duna: Parte Dois");
    assert_eq!(movie.imdb_id.as_deref(), Some("tt15239678"));
    assert_eq!(movie.runtime_minutes, Some(166));
    // This is the date that decides when searching stops being wasted.
    assert_eq!(movie.digital_release, Some(date!(2024 - 05 - 21)));
}

#[tokio::test]
async fn the_preferred_country_changes_which_digital_date_wins() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/movie/693134"))
        .respond_with(json(&fixture("movie_693134.json")))
        .mount(&server)
        .await;

    let movie = TmdbClient::new(FAKE_V4_TOKEN)
        .unwrap()
        .with_base_url(&server.uri())
        .unwrap()
        .with_retry(RetryConfig::disabled())
        .with_country("US")
        .movie(693_134)
        .await
        .unwrap();

    // The US block lists two type-4 entries; the first one wins, and it
    // is five weeks earlier than the Brazilian window.
    assert_eq!(movie.digital_release, Some(date!(2024 - 04 - 16)));
}

#[tokio::test]
async fn tv_details_expose_tvdb_id_and_sorted_seasons() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/tv/76479"))
        .and(query_param(
            "append_to_response",
            "external_ids,translations",
        ))
        .respond_with(json(&fixture("tv_76479.json")))
        .mount(&server)
        .await;

    let show = client(&server).tv(76479).await.unwrap();

    assert_eq!(show.name, "The Boys");
    assert_eq!(show.imdb_id.as_deref(), Some("tt1190634"));
    assert_eq!(
        show.tvdb_id,
        Some(355_567),
        "series carry a tvdb id; movies never do"
    );

    // The show has wrapped, and a finished series exposes exactly the
    // shape a naive parser trips on: no next episode, and an *empty*
    // episode_run_time array rather than a missing field.
    assert_eq!(show.status.as_deref(), Some("Ended"));
    assert!(!show.in_production);
    assert_eq!(show.next_air_date, None);
    assert_eq!(show.episode_runtime, None);

    let numbers: Vec<i32> = show.seasons.iter().map(|s| s.season_number).collect();
    assert_eq!(numbers, vec![0, 1, 2, 3, 4, 5]);
    assert_eq!(
        show.seasons[0].episode_count, 76,
        "season 0 is the specials bucket and is far bigger than a real season"
    );
    assert_eq!(show.seasons[1].episode_count, 8);
}

#[tokio::test]
async fn season_details_keep_unaired_episodes_with_no_date() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/tv/76479/season/4"))
        .respond_with(json(&fixture("season_4.json")))
        .mount(&server)
        .await;

    let season = client(&server).season(76_479, 4).await.unwrap();

    assert_eq!(season.season_number, 4);
    assert_eq!(season.air_date, Some(date!(2024 - 06 - 13)));
    assert_eq!(season.episodes.len(), 8);
    assert_eq!(season.episodes[0].episode_number, 1);
    assert_eq!(
        season.episodes[0].title.as_deref(),
        Some("Departamento de Truques Sujos")
    );
    assert_eq!(season.episodes[0].air_date, Some(date!(2024 - 06 - 13)));
    assert!(
        season.episodes.iter().all(|e| e.season_number == 4),
        "every episode reports the season it belongs to"
    );
}

#[tokio::test]
async fn find_by_imdb_resolves_to_a_movie() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/find/tt0133093"))
        .and(query_param("external_source", "imdb_id"))
        .respond_with(json(&fixture("find_imdb.json")))
        .mount(&server)
        .await;

    let found = client(&server).find_by_imdb("tt0133093").await.unwrap();

    assert_eq!(found.movies.len(), 1);
    assert!(found.series.is_empty());
    assert_eq!(found.movies[0].tmdb_id, 603);
}

#[tokio::test]
async fn find_by_tvdb_uses_the_right_external_source() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/find/355567"))
        .and(query_param("external_source", "tvdb_id"))
        .respond_with(json(r#"{"movie_results":[],"tv_results":[]}"#))
        .expect(1)
        .mount(&server)
        .await;

    client(&server).find_by_tvdb(355_567).await.unwrap();
}

#[tokio::test]
async fn a_401_reports_as_unauthorized_not_a_generic_http_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/movie/603"))
        .respond_with(ResponseTemplate::new(401).set_body_raw(
            r#"{"status_code":7,"status_message":"Invalid API key."}"#,
            "application/json",
        ))
        .mount(&server)
        .await;

    let err = client(&server).movie(603).await.unwrap_err();
    // The v3 key and the v4 read token are easy to confuse; the operator
    // needs to be told which one is wrong, not just "HTTP error".
    assert!(matches!(err, TmdbError::Unauthorized), "got {err:?}");
}

#[tokio::test]
async fn a_404_names_what_was_missing() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/movie/999999999"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let err = client(&server).movie(999_999_999).await.unwrap_err();
    match err {
        TmdbError::NotFound(what) => assert!(what.contains("999999999"), "got {what}"),
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[tokio::test]
async fn a_401_is_not_retried() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/movie/603"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;

    let c = TmdbClient::new("t")
        .unwrap()
        .with_base_url(&server.uri())
        .unwrap()
        .with_retry(RetryConfig {
            max_attempts: 4,
            base_delay: std::time::Duration::from_millis(0),
        });
    assert!(c.movie(603).await.is_err());
    // `.expect(1)` above is the real assertion: a wrong token must not
    // hammer TMDB four times.
}

#[tokio::test]
async fn a_429_backs_off_and_then_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/movie/603"))
        .respond_with(ResponseTemplate::new(429))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/movie/603"))
        .respond_with(json(&fixture("movie_603.json")))
        .expect(1)
        .mount(&server)
        .await;

    let c = TmdbClient::new("t")
        .unwrap()
        .with_base_url(&server.uri())
        .unwrap()
        .with_retry(RetryConfig {
            max_attempts: 3,
            base_delay: std::time::Duration::from_millis(0),
        });
    let movie = c.movie(603).await.unwrap();
    assert_eq!(movie.tmdb_id, 603);
}

#[tokio::test]
async fn a_5xx_is_retried_then_gives_up() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/movie/603"))
        .respond_with(ResponseTemplate::new(503))
        .expect(3)
        .mount(&server)
        .await;

    let c = TmdbClient::new("t")
        .unwrap()
        .with_base_url(&server.uri())
        .unwrap()
        .with_retry(RetryConfig {
            max_attempts: 3,
            base_delay: std::time::Duration::from_millis(0),
        });
    assert!(c.movie(603).await.is_err());
}

#[tokio::test]
async fn malformed_json_reports_as_bad_json() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/movie/603"))
        .respond_with(json("{ not json at all"))
        .mount(&server)
        .await;

    let c = client(&server);
    assert!(matches!(c.movie(603).await, Err(TmdbError::BadJson(_))));
}

#[tokio::test]
async fn verify_token_round_trips_configuration() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/configuration"))
        .respond_with(json(
            r#"{"images":{"base_url":"http://image.tmdb.org/t/p/"}}"#,
        ))
        .expect(1)
        .mount(&server)
        .await;

    client(&server).verify_token().await.unwrap();
}

#[tokio::test]
async fn language_override_reaches_the_wire() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search/movie"))
        .and(query_param("language", "en-US"))
        .respond_with(json(&fixture("search_movie_duna.json")))
        .expect(1)
        .mount(&server)
        .await;

    TmdbClient::new("t")
        .unwrap()
        .with_base_url(&server.uri())
        .unwrap()
        .with_retry(RetryConfig::disabled())
        .with_language("en-US")
        .search_movies("duna", None)
        .await
        .unwrap();
}
