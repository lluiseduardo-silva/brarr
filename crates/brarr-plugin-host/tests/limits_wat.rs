//! Sandbox enforcement tests: a misbehaving plugin should be killed
//! by the host instead of bringing down the worker.
//!
//! Two scenarios:
//! 1. **Infinite loop** — `plugin_search_by_tmdb` enters a `loop`/`br`
//!    cycle. With epoch interruption + a small deadline the host
//!    traps the call instead of hanging forever.
//! 2. **Memory grow over the cap** — plugin calls `memory.grow` with
//!    a page count above `HostCapabilities::max_memory_pages`. The
//!    `ResourceLimiter` impl on `MemoryLimiter` denies the growth, so
//!    `memory.grow` returns `-1` to the plugin (standard wasm
//!    semantics); plugin propagates the failure as a non-zero rc.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::doc_markdown,
    clippy::panic
)]

use std::time::Duration;

use brarr_core::{TmdbId, TrackerProvider, TrackerSource};
use brarr_plugin_host::{HostCapabilities, PluginConfig, WasmTrackerProvider};
use url::Url;

// Plugin whose search function spins forever. Everything else is the
// boilerplate ABI v1.
const INFINITE_LOOP_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (data (i32.const 0) "loopy")
  (global $bump (mut i32) (i32.const 256))
  (func (export "plugin_abi_version") (result i32) i32.const 1)
  (func (export "plugin_name") (result i64)
    i64.const 5 i64.const 32 i64.shl i64.const 0 i64.or)
  (func (export "plugin_alloc") (param $size i32) (result i32)
    (local $ptr i32)
    global.get $bump local.set $ptr
    global.get $bump local.get $size i32.add global.set $bump
    local.get $ptr)
  (func (export "plugin_free") (param i32) (param i32))
  (func (export "plugin_search_by_tmdb") (param i32 i32) (result i32)
    (loop $forever
      br $forever)
    i32.const 0)
)
"#;

// Plugin that immediately tries to grow memory by a number of pages
// above any sane cap, then surfaces the grow result as the search rc.
// `memory.grow` returns `-1` (as a 32-bit value, so `0xFFFF_FFFF`)
// when the host denies it; we propagate that.
const MEMORY_BOMB_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (data (i32.const 0) "bomb")
  (global $bump (mut i32) (i32.const 256))
  (func (export "plugin_abi_version") (result i32) i32.const 1)
  (func (export "plugin_name") (result i64)
    i64.const 4 i64.const 32 i64.shl i64.const 0 i64.or)
  (func (export "plugin_alloc") (param $size i32) (result i32)
    (local $ptr i32)
    global.get $bump local.set $ptr
    global.get $bump local.get $size i32.add global.set $bump
    local.get $ptr)
  (func (export "plugin_free") (param i32) (param i32))
  (func (export "plugin_search_by_tmdb") (param i32 i32) (result i32)
    ;; Try to grow by 100k pages (~6.4 GiB). Cap is way under that.
    i32.const 100000
    memory.grow
    ;; memory.grow returns -1 on failure, new page count on success.
    ;; We return that to the host so the test can assert.
  )
)
"#;

fn tracker() -> TrackerSource {
    TrackerSource::new("limits-tracker", Url::parse("https://t.example/").unwrap()).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn infinite_loop_plugin_is_trapped_by_epoch_deadline() {
    let bytes = wat::parse_str(INFINITE_LOOP_WAT).expect("compile wat");
    // Aggressive 300ms deadline so the test stays fast.
    let caps = HostCapabilities {
        call_deadline: Duration::from_millis(300),
        ..HostCapabilities::default()
    };
    let provider = WasmTrackerProvider::load_bytes(
        &bytes,
        PluginConfig::new(tracker()).with_capabilities(caps),
    )
    .await
    .expect("load");

    let started = std::time::Instant::now();
    let err = provider
        .search_by_tmdb(TmdbId::new(1).unwrap())
        .await
        .unwrap_err();
    let elapsed = started.elapsed();

    // Must finish well before a wall-clock minute even on slow CI.
    assert!(
        elapsed < Duration::from_secs(5),
        "plugin should have trapped under 5s, took {elapsed:?}"
    );
    // The wasmtime trap surfaces through PluginError::Wasm.
    let msg = err.to_string().to_ascii_lowercase();
    assert!(
        msg.contains("epoch") || msg.contains("interrupt") || msg.contains("wasm"),
        "expected trap-related message, got {msg}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_grow_over_cap_is_denied() {
    let bytes = wat::parse_str(MEMORY_BOMB_WAT).expect("compile wat");
    let caps = HostCapabilities {
        // Cap = 2 pages (128 KiB). Plugin asks for 100k. Must be denied.
        max_memory_pages: 2,
        ..HostCapabilities::default()
    };
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
    // memory.grow returned -1, plugin propagated it as the rc.
    // PluginError::PluginCode(-1) renders as "plugin returned error code -1".
    let msg = err.to_string();
    assert!(
        msg.contains("-1"),
        "expected memory.grow denial (-1), got {msg}"
    );
}
