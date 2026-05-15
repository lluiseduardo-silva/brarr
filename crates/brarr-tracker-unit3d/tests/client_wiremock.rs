//! Testes de integração HTTP usando [`wiremock`] como servidor mock.
//!
//! Exercita o caminho completo `Unit3dClient` → reqwest → servidor →
//! envelope JSON → DTO → conversão para `Release`. Cobre:
//! - Que `search_by_tmdb` monta a URL correta com query param `tmdbId`
//! - Que o envelope `{ "data": [...] }` é desembrulhado
//! - Que cada DTO da lista vira `Release` com enrichment populado a
//!   partir do `media_info`
//! - Que `get_torrent` desembrulha o envelope `{ "data": ... }` singleton

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::PathBuf;

use brarr_core::{Language, TmdbId, TrackerSource};
use brarr_tracker_unit3d::Unit3dClient;
use url::Url;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("docs")
        .join("requests-response-examples")
        .join(name);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("read fixture {}: {e}", p.display()))
}

/// Empacota o JSON cru de um torrent (sem envelope) num envelope de
/// **lista** — formato que `/api/torrents/filter` retorna.
fn wrap_as_filter_response(torrent_json: &str) -> String {
    format!(r#"{{"data": [{torrent_json}]}}"#)
}

/// Empacota o JSON cru de um torrent num envelope **singleton** —
/// formato que `/api/torrents/{{id}}` retorna.
fn wrap_as_single_response(torrent_json: &str) -> String {
    format!(r#"{{"data": {torrent_json}}}"#)
}

fn client_for(server: &MockServer, name: &str) -> Unit3dClient {
    // O cliente usa `base_url.join()` que requer trailing slash para que o
    // path relativo (e.g., `api/torrents/filter`) seja anexado em vez de
    // substituir o último segmento.
    let base = Url::parse(&format!("{}/", server.uri())).expect("valid mock URL");
    let tracker = TrackerSource::new(name, base).expect("non-empty");
    Unit3dClient::new(tracker, "test-token").expect("client")
}

#[tokio::test]
async fn search_by_tmdb_hits_filter_endpoint_with_query_param() {
    let server = MockServer::start().await;

    let body = wrap_as_filter_response(&fixture("shadow.json"));
    Mock::given(method("GET"))
        .and(path("/api/torrents/filter"))
        .and(query_param("tmdbId", "603"))
        .and(header("authorization", "Bearer test-token"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .expect(1)
        .mount(&server)
        .await;

    let client = client_for(&server, "mock");
    let releases = client
        .search_by_tmdb(TmdbId::new(603).expect("valid"))
        .await
        .expect("search");

    assert_eq!(releases.len(), 1);
    let r = &releases[0];
    assert_eq!(r.tracker_release_id, "125");
    assert_eq!(r.tracker.name, "mock");
    let e = r.enrichment.as_ref().expect("enrichment populated");
    assert!(e.has_audio_in(&Language::PtBr));
    assert!(e.has_hdr);
}

#[tokio::test]
async fn get_torrent_hits_singular_endpoint() {
    let server = MockServer::start().await;

    let body = wrap_as_single_response(&fixture("vnlls.json"));
    Mock::given(method("GET"))
        .and(path("/api/torrents/27582"))
        .and(header("authorization", "Bearer test-token"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .expect(1)
        .mount(&server)
        .await;

    let client = client_for(&server, "mock");
    let release = client.get_torrent("27582").await.expect("fetch");
    assert_eq!(release.tracker_release_id, "27582");
    assert_eq!(release.year, Some(1999));
    let e = release.enrichment.as_ref().expect("enrichment populated");
    assert!(e.has_audio_in(&Language::PtBr));
    assert!(!e.has_hdr); // vnlls é SDR
    assert_eq!(e.subtitle_count_in(&Language::PtPt), 1);
}

#[tokio::test]
async fn search_returns_empty_vec_when_envelope_is_empty() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/torrents/filter"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":[]}"#))
        .expect(1)
        .mount(&server)
        .await;

    let client = client_for(&server, "mock");
    let releases = client
        .search_by_tmdb(TmdbId::new(999_999).expect("valid"))
        .await
        .expect("empty search");
    assert!(releases.is_empty());
}

#[tokio::test]
async fn http_404_surfaces_as_client_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/torrents/nope"))
        .respond_with(ResponseTemplate::new(404).set_body_string(r#"{"error":"not found"}"#))
        .mount(&server)
        .await;

    let client = client_for(&server, "mock");
    let err = client
        .get_torrent("nope")
        .await
        .expect_err("expected error");
    let msg = format!("{err}");
    assert!(
        msg.contains("HTTP error") || msg.contains("404"),
        "unexpected error string: {msg}",
    );
}
