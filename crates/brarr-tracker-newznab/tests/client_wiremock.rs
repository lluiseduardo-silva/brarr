//! Integration: spin a wiremock server, point [`NewznabClient`] at it,
//! and verify end-to-end the request shape (query params) and the
//! response decoding.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::doc_markdown)]

use brarr_core::{ImdbId, TrackerProvider, TrackerSource};
use brarr_tracker_newznab::NewznabClient;
use url::Url;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MOVIE_FEED: &str = r#"<?xml version="1.0"?>
<rss xmlns:newznab="http://www.newznab.com/DTD/2010/feeds/attributes/">
  <channel>
    <newznab:response offset="0" total="1"/>
    <item>
      <title>Test.Movie.2024.1080p.BluRay.x264-FOO</title>
      <guid>abc</guid>
      <enclosure url="https://idx.example/api?t=get&amp;id=xyz" length="1234567890" type="application/x-nzb"/>
      <newznab:attr name="size" value="1234567890"/>
      <newznab:attr name="grabs" value="7"/>
      <newznab:attr name="imdb" value="9999001"/>
      <newznab:attr name="language" value="Portuguese (BR)"/>
      <newznab:attr name="audio" value="English"/>
      <newznab:attr name="subs" value="English"/>
    </item>
  </channel>
</rss>"#;

fn tracker_for(server: &MockServer) -> TrackerSource {
    TrackerSource::new("mock-nzbgeek", Url::parse(&server.uri()).unwrap())
        .unwrap()
        .with_base_url_trailing_slash()
}

// Local helper because TrackerSource doesn't expose a "ensure trailing
// slash" method; wiremock's URI never ends in `/`, but `base.join("api")`
// needs one. We just rebuild the URL.
trait TrackerSourceExt {
    fn with_base_url_trailing_slash(self) -> Self;
}
impl TrackerSourceExt for TrackerSource {
    fn with_base_url_trailing_slash(self) -> Self {
        let mut s = self.base_url.to_string();
        if !s.ends_with('/') {
            s.push('/');
        }
        TrackerSource::new(self.name.clone(), Url::parse(&s).unwrap()).unwrap()
    }
}

#[tokio::test]
async fn search_by_imdb_hits_movie_endpoint_with_imdbid_param() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("t", "movie"))
        .and(query_param("apikey", "test-key"))
        .and(query_param("imdbid", "9999001"))
        .respond_with(ResponseTemplate::new(200).set_body_string(MOVIE_FEED))
        .mount(&server)
        .await;

    let client = NewznabClient::new(tracker_for(&server), "test-key").expect("build");
    let releases = client
        .search_by_imdb(ImdbId::new(9_999_001).unwrap())
        .await
        .expect("search");
    assert_eq!(releases.len(), 1);
    let r = &releases[0];
    assert_eq!(r.title, "Test.Movie.2024.1080p.BluRay.x264-FOO");
    assert_eq!(r.size_bytes, 1_234_567_890);
    assert_eq!(r.snatches, 7);
    assert_eq!(r.tracker.name, "mock-nzbgeek");
    let e = r.enrichment.as_ref().unwrap();
    assert!(e.audio_languages.contains(&brarr_core::Language::PtBr));
    assert!(e.audio_languages.contains(&brarr_core::Language::En));
}

#[tokio::test]
async fn empty_channel_returns_empty_vec() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"<?xml version="1.0"?><rss><channel></channel></rss>"#),
        )
        .mount(&server)
        .await;
    let client = NewznabClient::new(tracker_for(&server), "k").unwrap();
    let releases = client
        .search_by_imdb(ImdbId::new(1).unwrap())
        .await
        .unwrap();
    assert!(releases.is_empty());
}

#[tokio::test]
async fn http_500_propagates_as_client_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let client = NewznabClient::new(tracker_for(&server), "k").unwrap();
    let err = client
        .search_by_imdb(ImdbId::new(1).unwrap())
        .await
        .unwrap_err();
    // ProviderError wraps the underlying ClientError's display.
    assert!(
        err.to_string().to_ascii_lowercase().contains("500"),
        "expected 500 in error, got {err}"
    );
}

#[tokio::test]
async fn tvsearch_hits_tvsearch_endpoint_with_tvdbid_season_ep() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(query_param("t", "tvsearch"))
        .and(query_param("tvdbid", "12345"))
        .and(query_param("season", "2"))
        .and(query_param("ep", "5"))
        .respond_with(ResponseTemplate::new(200).set_body_string(MOVIE_FEED))
        .mount(&server)
        .await;
    let client = NewznabClient::new(tracker_for(&server), "k").unwrap();
    let releases = client
        .search_by_tvdb(brarr_core::TvdbId::new(12345).unwrap(), Some(2), Some(5))
        .await
        .unwrap();
    assert_eq!(releases.len(), 1);
}

#[tokio::test]
async fn tvsearch_omits_season_and_ep_when_unspecified() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(query_param("t", "tvsearch"))
        .and(query_param("tvdbid", "777"))
        .respond_with(ResponseTemplate::new(200).set_body_string(MOVIE_FEED))
        .mount(&server)
        .await;
    let client = NewznabClient::new(tracker_for(&server), "k").unwrap();
    let releases = client
        .search_by_tvdb(brarr_core::TvdbId::new(777).unwrap(), None, None)
        .await
        .unwrap();
    assert_eq!(releases.len(), 1);
}

#[tokio::test]
async fn rejects_bad_apikey() {
    let url = Url::parse("https://api.example/").unwrap();
    let tracker = TrackerSource::new("x", url).unwrap();
    let err = NewznabClient::new(tracker, "key with spaces").unwrap_err();
    assert!(matches!(
        err,
        brarr_tracker_newznab::ClientError::InvalidApiKey
    ));
}
