//! Teste de integração end-to-end do pipeline de busca da CLI:
//! `run_search` contra dois servidores `wiremock` distintos (um pra
//! cada "tracker"), exercitando o paralelismo e a agregação +
//! ordenação por score.
//!
//! Reusa os JSONs reais (`docs/requests-response-examples/shadow.json`
//! e `vnlls.json`) como bodies devolvidos pelos mocks, dentro do
//! envelope `{"data": [...]}` que UNIT3D usa em `/api/torrents/filter`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::PathBuf;

use brarr_cli::{Engine, TrackerConfig, run_search};
use brarr_core::TmdbId;
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

fn wrap_filter(json: &str) -> String {
    format!(r#"{{"data": [{json}]}}"#)
}

fn mount_filter_returning(_server: &MockServer, body: &str, token: &str) -> wiremock::Mock {
    Mock::given(method("GET"))
        .and(path("/api/torrents/filter"))
        .and(query_param("tmdbId", "603"))
        .and(header("authorization", &format!("Bearer {token}")))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
}

fn tracker_for(server: &MockServer, name: &str, token: &str) -> TrackerConfig {
    TrackerConfig {
        name: name.to_string(),
        base_url: Url::parse(&format!("{}/", server.uri())).expect("valid mock URL"),
        token: token.to_string(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_aggregates_two_trackers_and_orders_by_score() {
    let capybara_server = MockServer::start().await;
    let locadora_server = MockServer::start().await;

    mount_filter_returning(
        &capybara_server,
        &wrap_filter(&fixture("shadow.json")),
        "tok-capy",
    )
    .expect(1)
    .mount(&capybara_server)
    .await;

    mount_filter_returning(
        &locadora_server,
        &wrap_filter(&fixture("vnlls.json")),
        "tok-loc",
    )
    .expect(1)
    .mount(&locadora_server)
    .await;

    let trackers = vec![
        tracker_for(&capybara_server, "capybara", "tok-capy"),
        tracker_for(&locadora_server, "locadora", "tok-loc"),
    ];

    let outcome = run_search(
        &trackers,
        TmdbId::new(603).expect("valid"),
        &Engine::baseline(),
    )
    .await
    .expect("search ok");

    assert_eq!(outcome.scored.len(), 2, "expected two releases total");
    assert!(outcome.failures.is_empty(), "no tracker failed");

    // Score top deve ser >= 2nd, ordenação descendente.
    let top = &outcome.scored[0];
    let second = &outcome.scored[1];
    assert!(
        top.score() >= second.score(),
        "top score {} should be >= second {}",
        top.score().get(),
        second.score().get(),
    );

    // Shadow (2160p HDR + PT-BR audio) deve vencer Vnlls (1080p SDR
    // mas com PT-BR audio + PT-BR subs full + PT-PT sub). Vnlls tem
    // mais subs, mas Shadow ganha pelo 2160p+HDR.
    // Ordem invariante depende exatamente dos pesos default — vamos
    // verificar identidades via id em vez de tentar adivinhar ordem.
    let ids: Vec<&str> = outcome
        .scored
        .iter()
        .map(|s| s.release.tracker_release_id.as_str())
        .collect();
    assert!(ids.contains(&"125"));
    assert!(ids.contains(&"27582"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tracker_failure_is_collected_not_fatal() {
    let good = MockServer::start().await;
    let bad = MockServer::start().await;

    mount_filter_returning(&good, &wrap_filter(&fixture("shadow.json")), "g")
        .mount(&good)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/torrents/filter"))
        .respond_with(ResponseTemplate::new(500).set_body_string(r#"{"error":"boom"}"#))
        .mount(&bad)
        .await;

    let trackers = vec![
        tracker_for(&good, "good", "g"),
        tracker_for(&bad, "bad", "b"),
    ];

    let outcome = run_search(
        &trackers,
        TmdbId::new(603).expect("valid"),
        &Engine::baseline(),
    )
    .await
    .expect("search ok overall");

    assert_eq!(outcome.scored.len(), 1, "only the good tracker produced");
    assert_eq!(outcome.failures.len(), 1, "bad tracker is reported");
    assert_eq!(outcome.failures[0].0, "bad");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_with_no_results_returns_empty_outcome() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/torrents/filter"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":[]}"#))
        .mount(&server)
        .await;

    let trackers = vec![tracker_for(&server, "solo", "tk")];
    let outcome = run_search(
        &trackers,
        TmdbId::new(1).expect("valid"),
        &Engine::baseline(),
    )
    .await
    .expect("search ok");

    assert!(outcome.scored.is_empty());
    assert!(outcome.failures.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn format_outcome_includes_pt_br_flag_for_shadow_release() {
    let server = MockServer::start().await;
    mount_filter_returning(&server, &wrap_filter(&fixture("shadow.json")), "tk")
        .mount(&server)
        .await;
    let trackers = vec![tracker_for(&server, "capy", "tk")];
    let outcome = run_search(
        &trackers,
        TmdbId::new(603).expect("valid"),
        &Engine::baseline(),
    )
    .await
    .expect("search ok");

    let rendered = brarr_cli::search::format_outcome(&outcome, 10);
    assert!(
        rendered.contains("PT-BR audio"),
        "rendered output should flag PT-BR audio; got:\n{rendered}",
    );
    assert!(
        rendered.contains("HDR"),
        "rendered output should flag HDR; got:\n{rendered}",
    );
    // Header
    assert!(rendered.contains("Top 1 de 1"));
}
