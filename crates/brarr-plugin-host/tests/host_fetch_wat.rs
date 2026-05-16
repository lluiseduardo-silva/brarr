//! Integration test for the `host_fetch` plugin capability.
//!
//! A hand-written .wat plugin makes a real HTTP GET against `wiremock`
//! (via the host-provided `host_fetch` import), reads the response body
//! out of plugin memory, and surfaces it as the plugin's search reply.
//!
//! Three scenarios:
//! 1. happy path — host serves valid release JSON, plugin returns one
//!    decoded [`brarr_core::Release`].
//! 2. fetch disabled — even with `host_fetch` imported, plugin gets
//!    `-2` from the host because `HostCapabilities::fetch == false`.
//! 3. host not in allowlist — `-3` returned.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::doc_markdown,
    clippy::panic
)]

use std::sync::Arc;

use brarr_core::{TmdbId, TrackerProvider, TrackerSource};
use brarr_plugin_host::{HostCapabilities, PluginConfig, WasmTrackerProvider};
use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const RESPONSE_JSON: &str =
    "[{\"id\":\"7\",\"title\":\"fetched\",\"size_bytes\":99,\"resolution\":\"720p\"}]";

fn plugin_wat(url: &str) -> String {
    let url_len = url.len();
    format!(
        r#"
(module
  (import "env" "host_fetch"
    (func $host_fetch (param i32 i32 i32 i32 i32 i32) (result i32)))

  (memory (export "memory") 1)

  ;; [0..12)   plugin_name = "fetch-test\0\0" (12 bytes, lower 10 used)
  ;; [16..16+URL_LEN) URL bytes
  ;; [512..520) scratch for host_fetch out_handle
  (data (i32.const 0)  "fetch-test")
  (data (i32.const 16) "{url}")

  (global $bump (mut i32) (i32.const 1024))

  (func (export "plugin_abi_version") (result i32)
    i32.const 1)

  (func (export "plugin_name") (result i64)
    i64.const 10        ;; len of "fetch-test"
    i64.const 32
    i64.shl
    i64.const 0
    i64.or)

  (func (export "plugin_alloc") (param $size i32) (result i32)
    (local $ptr i32)
    global.get $bump
    local.set $ptr
    global.get $bump
    local.get $size
    i32.add
    global.set $bump
    local.get $ptr)

  (func (export "plugin_free") (param $ptr i32) (param $len i32))

  (func (export "plugin_search_by_tmdb")
        (param $tmdb i32) (param $out_handle i32) (result i32)
    (local $rc i32)
    (local $body_ptr i32)
    (local $body_len i32)

    ;; host_fetch(0=GET, url_ptr=16, url_len, body_ptr=0, body_len=0, fetch_h=512)
    i32.const 0
    i32.const 16
    i32.const {url_len}
    i32.const 0
    i32.const 0
    i32.const 512
    call $host_fetch
    local.set $rc

    ;; If status != 200, propagate non-zero rc.
    local.get $rc
    i32.const 200
    i32.ne
    if
      local.get $rc
      return
    end

    ;; Read (body_ptr, body_len) from 512..520 set by host.
    i32.const 512
    i32.load
    local.set $body_ptr
    i32.const 516
    i32.load
    local.set $body_len

    ;; Write (body_ptr, body_len) to out_handle for host to read.
    local.get $out_handle
    local.get $body_ptr
    i32.store
    local.get $out_handle
    i32.const 4
    i32.add
    local.get $body_len
    i32.store

    i32.const 0)
)
"#
    )
}

fn tracker() -> TrackerSource {
    TrackerSource::new("fetch-tracker", Url::parse("https://t.example/").unwrap()).unwrap()
}

#[tokio::test]
async fn host_fetch_happy_path_returns_release() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/torrents"))
        .respond_with(ResponseTemplate::new(200).set_body_string(RESPONSE_JSON))
        .mount(&server)
        .await;

    let url = format!("{}/torrents", server.uri());
    let wat = plugin_wat(&url);
    let bytes = wat::parse_str(&wat).expect("compile wat");

    let mock_host = Url::parse(&server.uri())
        .unwrap()
        .host_str()
        .unwrap()
        .to_string();
    let caps = HostCapabilities::default().with_fetch([mock_host]);

    let provider = WasmTrackerProvider::load_bytes(
        &bytes,
        PluginConfig::new(tracker())
            .with_capabilities(caps)
            .with_http(Arc::new(reqwest::Client::new())),
    )
    .await
    .expect("load");

    let releases = provider
        .search_by_tmdb(TmdbId::new(1).unwrap())
        .await
        .expect("search");
    assert_eq!(releases.len(), 1);
    let r = &releases[0];
    assert_eq!(r.tracker_release_id, "7");
    assert_eq!(r.title, "fetched");
    assert_eq!(r.size_bytes, 99);
    assert_eq!(r.tracker.name, "fetch-tracker");
}

#[tokio::test]
async fn host_fetch_disabled_returns_minus2_code() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string(RESPONSE_JSON))
        .mount(&server)
        .await;

    let url = format!("{}/torrents", server.uri());
    let wat = plugin_wat(&url);
    let bytes = wat::parse_str(&wat).expect("compile wat");

    // Default capabilities → fetch = false.
    let provider = WasmTrackerProvider::load_bytes(&bytes, PluginConfig::new(tracker()))
        .await
        .expect("load");

    let err = provider
        .search_by_tmdb(TmdbId::new(1).unwrap())
        .await
        .unwrap_err();
    // The plugin propagates the negative rc; PluginError::PluginCode wraps it.
    assert!(
        err.to_string().contains("-2"),
        "expected fetch-disabled (-2) in error, got {err}"
    );
}

#[tokio::test]
async fn host_fetch_host_not_in_allowlist_returns_minus3_code() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string(RESPONSE_JSON))
        .mount(&server)
        .await;

    let url = format!("{}/torrents", server.uri());
    let wat = plugin_wat(&url);
    let bytes = wat::parse_str(&wat).expect("compile wat");

    // fetch=true but allowlist contains an unrelated host.
    let caps = HostCapabilities::default().with_fetch(["nope.example"]);

    let provider = WasmTrackerProvider::load_bytes(
        &bytes,
        PluginConfig::new(tracker()).with_capabilities(caps),
    )
    .await
    .expect("load");

    let err = provider
        .search_by_tmdb(TmdbId::new(1).unwrap())
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("-3"),
        "expected host-blocked (-3) in error, got {err}"
    );
}
