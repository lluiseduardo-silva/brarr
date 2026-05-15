//! End-to-end test: build a hand-written `.wat` plugin, load it via
//! [`brarr_plugin_host::WasmTrackerProvider`], and exercise the full
//! ABI round-trip (name probe, ABI-version check, search call).
//!
//! The plugin is tiny but covers every ABI v1 requirement:
//! - Exports a linear memory pre-populated with a UTF-8 plugin name
//!   and a JSON response.
//! - `plugin_alloc` returns a fixed scratch region (no real allocator).
//! - `plugin_free` is a no-op.
//! - `plugin_abi_version` returns 1.
//! - `plugin_name` returns the packed (ptr, len) for "wat-test".
//! - `plugin_search_by_tmdb` writes (response_ptr, response_len) at the
//!   `out_handle` and returns 0.
//! - Imports `env.host_log` and calls it once on every search to prove
//!   the host capability path works.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown
)]

use brarr_core::{TmdbId, TrackerProvider, TrackerSource};
use brarr_plugin_host::{PluginConfig, WasmTrackerProvider};
use url::Url;

const PLUGIN_WAT: &str = r#"
(module
  (import "env" "host_log" (func $host_log (param i32 i32 i32)))

  (memory (export "memory") 1)

  ;; Static data layout in memory:
  ;;   [0..8)    bytes for plugin_name   -> "wat-test"
  ;;   [8..75)   JSON response           -> single-release array (67 bytes)
  ;;   [80..94)  log message             -> "search invoked" (14 bytes)
  ;;   [256]     scratch start for plugin_alloc
  (data (i32.const 0)  "wat-test")
  (data (i32.const 8)  "[{\"id\":\"42\",\"title\":\"test\",\"size_bytes\":1234,\"resolution\":\"1080p\"}]")
  (data (i32.const 80) "search invoked")

  ;; Single bump-pointer allocator that always returns address 256 and
  ;; advances by request size. Tests never request more than the page.
  (global $bump (mut i32) (i32.const 256))

  (func (export "plugin_abi_version") (result i32)
    i32.const 1
  )

  (func (export "plugin_name") (result i64)
    ;; Pack (len << 32) | ptr where ptr=0, len=8.
    i64.const 8
    i64.const 32
    i64.shl
    i64.const 0
    i64.or
  )

  (func (export "plugin_alloc") (param $size i32) (result i32)
    (local $ptr i32)
    global.get $bump
    local.set $ptr
    global.get $bump
    local.get $size
    i32.add
    global.set $bump
    local.get $ptr
  )

  (func (export "plugin_free") (param $ptr i32) (param $len i32)
    ;; no-op
  )

  (func (export "plugin_search_by_tmdb")
        (param $tmdb i32) (param $out_handle i32) (result i32)
    ;; Log: host_log(2 /* info */, 80, 14)
    i32.const 2
    i32.const 80
    i32.const 14
    call $host_log

    ;; Write ptr=8 at out_handle+0 (little-endian u32).
    local.get $out_handle
    i32.const 8
    i32.store

    ;; Write len=67 at out_handle+4 (length of the JSON above).
    local.get $out_handle
    i32.const 4
    i32.add
    i32.const 67
    i32.store

    i32.const 0
  )
)
"#;

fn tracker() -> TrackerSource {
    TrackerSource::new("wat-tracker", Url::parse("https://wat.example/").unwrap()).unwrap()
}

#[tokio::test]
async fn loads_wat_plugin_and_invokes_search() {
    let bytes = wat::parse_str(PLUGIN_WAT).expect("compile wat");
    let provider =
        WasmTrackerProvider::load_bytes(&bytes, PluginConfig::new(tracker())).expect("load");
    assert_eq!(provider.plugin_name(), "wat-test");
    assert_eq!(provider.name(), "wat-tracker");

    let releases = provider
        .search_by_tmdb(TmdbId::new(603).unwrap())
        .await
        .expect("search");
    assert_eq!(releases.len(), 1);
    let r = &releases[0];
    assert_eq!(r.tracker_release_id, "42");
    assert_eq!(r.title, "test");
    assert_eq!(r.size_bytes, 1234);
    assert_eq!(r.tracker.name, "wat-tracker");
}

#[tokio::test]
async fn missing_required_export_is_diagnosed() {
    // Module exports memory, abi_version, name, alloc, free, but no
    // plugin_search_by_tmdb.
    let bad = r#"
        (module
          (memory (export "memory") 1)
          (data (i32.const 0) "bad")
          (func (export "plugin_abi_version") (result i32) i32.const 1)
          (func (export "plugin_name") (result i64)
            i64.const 3 i64.const 32 i64.shl i64.const 0 i64.or)
          (func (export "plugin_alloc") (param i32) (result i32) i32.const 0)
          (func (export "plugin_free") (param i32) (param i32))
        )
    "#;
    let bytes = wat::parse_str(bad).expect("compile wat");
    let err = WasmTrackerProvider::load_bytes(&bytes, PluginConfig::new(tracker())).unwrap_err();
    match err {
        brarr_plugin_host::PluginError::MissingExport { name, .. } => {
            assert_eq!(name, "plugin_search_by_tmdb");
        }
        other => panic!("expected MissingExport, got {other:?}"),
    }
}

#[tokio::test]
async fn wrong_abi_version_rejected() {
    let bad = r#"
        (module
          (memory (export "memory") 1)
          (data (i32.const 0) "bad")
          (func (export "plugin_abi_version") (result i32) i32.const 99)
          (func (export "plugin_name") (result i64)
            i64.const 3 i64.const 32 i64.shl i64.const 0 i64.or)
          (func (export "plugin_alloc") (param i32) (result i32) i32.const 0)
          (func (export "plugin_free") (param i32) (param i32))
          (func (export "plugin_search_by_tmdb") (param i32 i32) (result i32) i32.const 0)
        )
    "#;
    let bytes = wat::parse_str(bad).expect("compile wat");
    let err = WasmTrackerProvider::load_bytes(&bytes, PluginConfig::new(tracker())).unwrap_err();
    match err {
        brarr_plugin_host::PluginError::UnsupportedAbi { got, supported } => {
            assert_eq!(got, 99);
            assert_eq!(supported, 1);
        }
        other => panic!("expected UnsupportedAbi, got {other:?}"),
    }
}
