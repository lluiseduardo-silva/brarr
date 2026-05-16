//! Integration test: persist a tracker row whose `plugin_path` points
//! at a hand-written `.wat` plugin, run a TMDb search through the
//! orchestrator, and assert the search pipeline:
//!
//! 1. Built a `TrackerProvider` for the row by loading the plugin.
//! 2. Invoked the plugin's `plugin_search_by_tmdb`.
//! 3. Decoded the plugin's JSON output and persisted one decision row.
//!
//! This is the cross-crate seam that proves Phase 6c integrates with
//! Phase 6b — the orchestrator can mix UNIT3D direct clients and WASM
//! plugin providers in the same fan-out via [`brarr_core::TrackerProvider`].

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::doc_markdown,
    clippy::panic
)]

use std::io::Write;

use brarr_decision_service::Engine;
use brarr_orchestrator::{AppState, db, search};
use url::Url;

const PLUGIN_WAT: &str = r#"
(module
  (import "env" "host_log" (func $host_log (param i32 i32 i32)))
  (memory (export "memory") 1)

  ;; [0..7)  plugin_name "orch-it"
  ;; [8..75) JSON response (67 bytes)
  ;; [80..)  scratch
  (data (i32.const 0)  "orch-it")
  (data (i32.const 8)  "[{\"id\":\"99\",\"title\":\"plug\",\"size_bytes\":2048,\"resolution\":\"1080p\"}]")

  (global $bump (mut i32) (i32.const 256))

  (func (export "plugin_abi_version") (result i32) i32.const 1)

  (func (export "plugin_name") (result i64)
    i64.const 7 i64.const 32 i64.shl i64.const 0 i64.or)

  (func (export "plugin_alloc") (param $size i32) (result i32)
    (local $ptr i32)
    global.get $bump
    local.set $ptr
    global.get $bump
    local.get $size
    i32.add
    global.set $bump
    local.get $ptr)

  (func (export "plugin_free") (param i32) (param i32))

  (func (export "plugin_search_by_tmdb")
        (param $tmdb i32) (param $out_handle i32) (result i32)
    local.get $out_handle
    i32.const 8
    i32.store
    local.get $out_handle
    i32.const 4
    i32.add
    i32.const 67
    i32.store
    i32.const 0)
)
"#;

#[tokio::test]
async fn search_runs_through_plugin_tracker() {
    // Compile the WAT to a binary wasm, write it to a temp file, then
    // insert a tracker row that points at it.
    let bytes = wat::parse_str(PLUGIN_WAT).expect("compile wat");
    let tmp_dir = std::env::temp_dir().join("brarr-orch-plugin-it");
    let _ = std::fs::create_dir_all(&tmp_dir);
    let wasm_path = tmp_dir.join("orch-it.wasm");
    {
        let mut f = std::fs::File::create(&wasm_path).expect("create wasm file");
        f.write_all(&bytes).expect("write wasm bytes");
    }

    let pool = db::open_memory().await.expect("db");
    let url = Url::parse("https://plugin.example/").expect("url");
    db::providers::insert(
        &pool,
        db::providers::NewProvider {
            name: "plugin-tracker",
            base_url: &url,
            api_token: "",
            kind: "plugin",
            plugin_path: Some(&wasm_path),
        },
    )
    .await
    .expect("insert provider row");

    let state = AppState::new(pool, Engine::baseline());
    let outcome = search::run_tmdb_search(&state, brarr_core::TmdbId::new(603).unwrap())
        .await
        .expect("search");

    assert!(
        outcome.failures.is_empty(),
        "expected no failures, got {:?}",
        outcome.failures
    );
    assert_eq!(outcome.decisions.len(), 1, "should persist one release");
    let d = &outcome.decisions[0];
    assert_eq!(d.provider_name, "plugin-tracker");
    assert_eq!(d.release_name, "plug");
    assert_eq!(d.size_bytes, 2048);
    assert_eq!(d.resolution, "1080p");
    assert_eq!(d.release_id_remote, 99);

    // search row was updated to reflect the persisted result count
    assert_eq!(outcome.search.result_count, 1);

    let _ = std::fs::remove_file(&wasm_path);
}

#[tokio::test]
async fn missing_plugin_file_collected_as_failure_not_aborted() {
    let pool = db::open_memory().await.expect("db");
    let url = Url::parse("https://plugin.example/").expect("url");
    let bogus_path = std::path::PathBuf::from("/nonexistent/path/to/plugin.wasm");
    db::providers::insert(
        &pool,
        db::providers::NewProvider {
            name: "bogus",
            base_url: &url,
            api_token: "",
            kind: "plugin",
            plugin_path: Some(&bogus_path),
        },
    )
    .await
    .expect("insert");

    let state = AppState::new(pool, Engine::baseline());
    let outcome = search::run_tmdb_search(&state, brarr_core::TmdbId::new(1).unwrap())
        .await
        .expect("search must not abort just because one plugin is broken");

    assert!(outcome.decisions.is_empty());
    assert_eq!(outcome.failures.len(), 1);
    assert_eq!(outcome.failures[0].0, "bogus");
    assert!(
        outcome.failures[0].1.contains("read plugin"),
        "failure msg = {:?}",
        outcome.failures[0].1
    );
}
