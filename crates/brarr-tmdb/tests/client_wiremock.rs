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

fn client(server: &MockServer) -> TmdbClient {
    TmdbClient::new("read-access-token")
        .unwrap()
        .with_base_url(&server.uri())
        .unwrap()
        .with_retry(RetryConfig::disabled())
}

fn json(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body.to_owned(), "application/json")
}

#[tokio::test]
async fn sends_the_token_as_a_bearer_header() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search/movie"))
        .and(header("authorization", "Bearer read-access-token"))
        .respond_with(json(&fixture("search_movie_duna.json")))
        .expect(1)
        .mount(&server)
        .await;

    client(&server).search_movies("duna", None).await.unwrap();
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

    assert_eq!(hits.len(), 3);
    assert_eq!(hits[0].tmdb_id, 693_134);
    assert_eq!(hits[0].title, "Duna: Parte Dois");
    assert_eq!(hits[0].year(), Some(2024));
    assert_eq!(
        hits[2].poster_path, None,
        "null poster must not fail the row"
    );
    assert_eq!(
        hits[2].overview, None,
        "empty-string overview reads as absent"
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
    assert_eq!(movie.release_date, Some(date!(1999 - 03 - 30)));
}

#[tokio::test]
async fn empty_overview_falls_back_to_the_pt_br_translation() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/movie/603"))
        .respond_with(json(&fixture("movie_603.json")))
        .mount(&server)
        .await;

    let movie = client(&server).movie(603).await.unwrap();

    // TMDB returned "" at the top level — the pt-BR text has to come out
    // of `translations`, not out of the English entry that sits first.
    assert_eq!(
        movie.overview.as_deref(),
        Some(
            "Um hacker descobre que a realidade em que vive é uma simulação e se junta à rebelião contra as máquinas."
        )
    );
}

#[tokio::test]
async fn release_dates_resolve_per_country_and_type() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/movie/603"))
        .respond_with(json(&fixture("movie_603.json")))
        .mount(&server)
        .await;

    let movie = client(&server).movie(603).await.unwrap();

    // BR is the default preferred country: digital is type 4, physical 5.
    assert_eq!(movie.digital_release, Some(date!(1999 - 11 - 05)));
    assert_eq!(movie.physical_release, Some(date!(2000 - 02 - 18)));
}

#[tokio::test]
async fn a_different_country_selects_a_different_digital_date() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/movie/603"))
        .respond_with(json(&fixture("movie_603.json")))
        .mount(&server)
        .await;

    let movie = TmdbClient::new("t")
        .unwrap()
        .with_base_url(&server.uri())
        .unwrap()
        .with_retry(RetryConfig::disabled())
        .with_country("US")
        .movie(603)
        .await
        .unwrap();

    assert_eq!(movie.digital_release, Some(date!(1999 - 09 - 21)));
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
    assert_eq!(show.status.as_deref(), Some("Returning Series"));
    assert!(show.in_production);
    assert_eq!(show.next_air_date, Some(date!(2026 - 08 - 12)));
    assert_eq!(show.episode_runtime, Some(60));

    let numbers: Vec<i32> = show.seasons.iter().map(|s| s.season_number).collect();
    assert_eq!(
        numbers,
        vec![0, 1, 2, 3, 4],
        "TMDB returns seasons unordered; the client sorts them"
    );
    assert_eq!(show.seasons[0].episode_count, 3, "season 0 is the specials");
}

#[tokio::test]
async fn season_details_keep_unaired_episodes_with_no_date() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/tv/76479/season/4"))
        .respond_with(json(&fixture("season_4.json")))
        .mount(&server)
        .await;

    let season = client(&server).season(76479, 4).await.unwrap();

    assert_eq!(season.season_number, 4);
    assert_eq!(season.episodes.len(), 3);
    assert_eq!(season.episodes[0].air_date, Some(date!(2024 - 06 - 13)));
    assert_eq!(
        season.episodes[2].air_date, None,
        "an unaired episode has air_date \"\" and must still be listed"
    );
    assert_eq!(
        season.episodes[2].title.as_deref(),
        Some("Ainda sem título")
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
