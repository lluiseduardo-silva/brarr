//! `brarr-plugin-host` — `WASM` sandbox for third-party tracker scrapers.
//!
//! Loads core WebAssembly modules through [`wasmtime`] and presents each
//! one as a [`brarr_core::TrackerProvider`], so the orchestrator can mix
//! direct UNIT3D clients with sandboxed plugins in the same fan-out.
//!
//! ## Plugin ABI v1
//!
//! Every plugin must be a **core wasm module** (not the Component Model
//! yet — kept deliberately simple). It must export the following symbols
//! and may import the documented host capabilities.
//!
//! ### Required exports
//!
//! | Name                       | Signature                          | Purpose                                                                 |
//! |----------------------------|------------------------------------|-------------------------------------------------------------------------|
//! | `memory`                   | `(memory)`                         | Plugin's linear memory; host reads/writes through it.                   |
//! | `plugin_alloc`             | `(param i32) (result i32)`         | Allocate `n` bytes inside the plugin; return pointer.                   |
//! | `plugin_free`              | `(param i32 i32)`                  | Free a previously-allocated region (`ptr`, `len`).                      |
//! | `plugin_abi_version`       | `(result i32)`                     | Return `1` for this version; future plugins bump.                       |
//! | `plugin_name`              | `(result i64)`                     | Packed result: low 32 bits = ptr, high 32 bits = len. Encodes plugin's display name (UTF-8).|
//! | `plugin_search_by_tmdb`    | `(param i32 i32) (result i32)`     | Args: `tmdb_id`, `out_handle` (8-byte region: ptr + len for JSON response). Returns `0` on success or a non-zero error code.|
//!
//! ### Importable host functions
//!
//! Plugins import these from module `env`. Capability gating is per-host
//! [`PluginConfig`] — the loader can disable any of them.
//!
//! | Name                | Signature                                       | Purpose                                                                 |
//! |---------------------|-------------------------------------------------|-------------------------------------------------------------------------|
//! | `host_log`          | `(param i32 i32 i32)`                           | Log at `level` (0=trace .. 4=error), message at `ptr`/`len` in plugin mem.|
//! | `host_fetch`        | `(param i32 i32 i32 i32 i32 i32) (result i32)`  | `(method, url_ptr, url_len, body_ptr, body_len, out_handle)`. Async HTTP request. Returns the HTTP status (`200`..`599`) on success or a negative error code (`-1` transport, `-2` capability disabled, `-3` host not in allowlist). On success the host allocates space via `plugin_alloc` and writes `(ptr, len)` of the response body at `out_handle`. Method enum: `0`=GET, `1`=POST, `2`=PUT, `3`=DELETE. |
//!
//! `host_kv_*` is reserved for future revisions.
//!
//! ### Response JSON shape
//!
//! `plugin_search_by_tmdb` must write a UTF-8 JSON array to a freshly
//! allocated region and store `(ptr, len)` at `out_handle`. Each entry
//! deserializes into [`PluginRelease`] (see `dto` module). The host
//! converts each entry into a [`brarr_core::Release`].
//!
//! ## Why core wasm + manual ABI?
//!
//! The Component Model is nicer ergonomically (typed strings, records),
//! but its tooling story is still in flux. Core wasm with an
//! explicit-but-tiny FFI is something any language with a wasm32 target
//! can produce, including hand-written `.wat`. The whole plugin contract
//! lives in this file's docs — that matters more than ergonomics for a
//! sandbox that runs untrusted code.

#![allow(
    clippy::module_name_repetitions,
    clippy::doc_markdown,
    reason = "TMDb/IMDb/TVDb/MyAnimeList appear in user-facing module docs frequently"
)]

pub mod dto;
pub mod error;
pub mod host;
pub mod plugin;

pub use error::{PluginError, PluginResult};
pub use host::{DEFAULT_FETCH_TIMEOUT, HostCapabilities};
pub use plugin::{PluginConfig, SUPPORTED_ABI_VERSION, WasmTrackerProvider};
