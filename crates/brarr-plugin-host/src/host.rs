//! Host-side state held by every plugin instance.
//!
//! The [`HostState`] is carried in `Store<HostState>` by wasmtime. Host
//! functions imported by the plugin receive a [`Caller`] from which
//! they unpack the [`HostState`] and the plugin memory.
//!
//! ## Host imports installed
//!
//! - `env.host_log(level: i32, ptr: i32, len: i32)` — synchronous log
//!   sink. Gated by [`HostCapabilities::log`].
//! - `env.host_fetch(method: i32, url_ptr: i32, url_len: i32,
//!                   body_ptr: i32, body_len: i32, out_handle: i32)
//!                   -> i32` — async HTTP request. Gated by
//!   [`HostCapabilities::fetch`] plus a per-host URL allowlist
//!   (`HostCapabilities::allowed_hosts`). Returns the HTTP status code
//!   (e.g. `200`, `404`, `500`) on success or a negative error code
//!   (`-1` for transport, `-2` for disabled, `-3` for blocked host)
//!   when the host could not deliver the request. On success, the host
//!   allocates a region in plugin memory via `plugin_alloc`, writes
//!   the response body there, and stores `(ptr, len)` as two
//!   little-endian `u32`s starting at `out_handle`.
//!
//! HTTP method enum (matches `i32` argument):
//!
//! | Value | Method |
//! |-------|--------|
//! | `0`   | GET    |
//! | `1`   | POST   |
//! | `2`   | PUT    |
//! | `3`   | DELETE |
//! | other | rejected with `-1` |

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use tracing::{Level, debug, event, warn};
use wasmtime::{AsContextMut, Caller, Extern, Linker, Memory, TypedFunc};

use crate::PluginError;

/// Default per-request timeout for `host_fetch`.
pub const DEFAULT_FETCH_TIMEOUT: Duration = Duration::from_secs(15);

/// Default wall-clock deadline per plugin call before the host traps
/// the wasm execution. Enforced via wasmtime's epoch interruption
/// machinery — see [`crate::WasmEpochTicker`].
pub const DEFAULT_CALL_DEADLINE: Duration = Duration::from_secs(5);

/// Default ceiling on the plugin's linear memory, in **wasm pages**
/// (64 KiB each). 1024 pages = 64 MiB. Plugins that try to grow past
/// this fail their `memory.grow` and likely trap.
pub const DEFAULT_MAX_MEMORY_PAGES: usize = 1024;

/// State threaded through every plugin call.
pub struct HostState {
    /// Display name of the plugin (logged with every `host_log` call).
    pub plugin_name: String,
    /// Capability flags.
    pub caps: HostCapabilities,
    /// Shared HTTP client used by `host_fetch`. Built once per
    /// `PluginConfig` so connection pooling works across calls.
    pub http: Arc<reqwest::Client>,
    /// Linear-memory ceiling enforcer plugged into wasmtime's
    /// [`wasmtime::ResourceLimiter`] hook. Mutated through the trait
    /// during plugin execution.
    pub limiter: MemoryLimiter,
}

/// What the host allows the plugin to do.
#[derive(Debug, Clone)]
pub struct HostCapabilities {
    /// Whether `host_log` is callable. Default `true`.
    pub log: bool,
    /// Whether `host_fetch` is callable. Default `false` — opt-in.
    pub fetch: bool,
    /// Hostnames the plugin is allowed to reach via `host_fetch`. An
    /// empty set means **deny all**; explicit wildcard is `"*"`.
    /// Comparisons are case-insensitive and exact (no subdomain
    /// matching). Default empty.
    pub allowed_hosts: HashSet<String>,
    /// Per-request timeout. Default [`DEFAULT_FETCH_TIMEOUT`].
    pub fetch_timeout: Duration,
    /// Wall-clock deadline applied to every plugin call. Implemented
    /// via wasmtime epoch interruption — a misbehaving plugin that
    /// burns CPU in a tight loop traps once the deadline elapses
    /// instead of pinning a worker forever.
    pub call_deadline: Duration,
    /// Maximum number of wasm memory pages (64 KiB each) the plugin
    /// can hold. Default [`DEFAULT_MAX_MEMORY_PAGES`] (= 64 MiB).
    pub max_memory_pages: usize,
}

impl Default for HostCapabilities {
    fn default() -> Self {
        Self {
            log: true,
            fetch: false,
            allowed_hosts: HashSet::new(),
            fetch_timeout: DEFAULT_FETCH_TIMEOUT,
            call_deadline: DEFAULT_CALL_DEADLINE,
            max_memory_pages: DEFAULT_MAX_MEMORY_PAGES,
        }
    }
}

/// Wasmtime [`ResourceLimiter`] impl that caps each plugin's linear
/// memory to `max_pages` wasm pages. Implemented as a plain struct
/// (no `dyn`) so it lives inside `HostState` without heap allocation.
#[derive(Debug, Clone, Copy)]
pub struct MemoryLimiter {
    /// Page ceiling honoured by [`wasmtime::ResourceLimiter::memory_growing`].
    pub max_pages: usize,
}

impl MemoryLimiter {
    /// Convenience: page count → byte count assuming 64 KiB pages.
    #[must_use]
    pub const fn max_bytes(self) -> usize {
        self.max_pages.saturating_mul(64 * 1024)
    }
}

impl wasmtime::ResourceLimiter for MemoryLimiter {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        Ok(desired <= self.max_bytes())
    }

    fn table_growing(
        &mut self,
        _current: usize,
        _desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        // Tables aren't a concern for our ABI; leave them uncapped.
        Ok(true)
    }
}

impl HostCapabilities {
    /// Enable fetch + accept a list of hostnames. Equivalent to setting
    /// `fetch = true` and filling `allowed_hosts`.
    #[must_use]
    pub fn with_fetch<I, S>(mut self, hosts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.fetch = true;
        self.allowed_hosts = hosts.into_iter().map(Into::into).collect();
        self
    }

    /// Check whether `host` is in the allowlist (case-insensitive).
    /// Returns `true` if the wildcard `"*"` is present.
    #[must_use]
    pub fn allows_host(&self, host: &str) -> bool {
        if self.allowed_hosts.contains("*") {
            return true;
        }
        let lower = host.to_ascii_lowercase();
        self.allowed_hosts
            .iter()
            .any(|h| h.eq_ignore_ascii_case(&lower))
    }
}

/// Install all host-side imports onto `linker` under the `env` module.
///
/// Capability checks happen *inside* each registered function so a
/// plugin trying to call a disabled function receives the documented
/// negative error code (for `host_fetch`) or a trap (for `host_log`).
///
/// # Errors
///
/// Propagates [`wasmtime::Error`] if `linker.func_wrap*` fails (only
/// when registering the same name twice).
pub fn install_imports(linker: &mut Linker<HostState>) -> Result<(), PluginError> {
    install_host_log(linker)?;
    install_host_fetch(linker)?;
    Ok(())
}

fn install_host_log(linker: &mut Linker<HostState>) -> Result<(), PluginError> {
    linker
        .func_wrap(
            "env",
            "host_log",
            |mut caller: Caller<'_, HostState>, level: i32, ptr: i32, len: i32| {
                if !caller.data().caps.log {
                    return Err(wasmtime::Error::msg("host_log: capability disabled"));
                }
                let Some(Extern::Memory(mem)) = caller.get_export("memory") else {
                    return Err(wasmtime::Error::msg("plugin did not export `memory`"));
                };
                let data = mem.data(&caller);
                let msg = read_string(data, ptr, len)?;
                let plugin_name = caller.data().plugin_name.clone();
                emit_log(level, &plugin_name, &msg);
                Ok(())
            },
        )
        .map_err(PluginError::from)?;
    Ok(())
}

fn install_host_fetch(linker: &mut Linker<HostState>) -> Result<(), PluginError> {
    linker
        .func_wrap_async(
            "env",
            "host_fetch",
            |mut caller: Caller<'_, HostState>,
             (method, url_ptr, url_len, body_ptr, body_len, out_handle): (
                i32,
                i32,
                i32,
                i32,
                i32,
                i32,
            )| {
                Box::new(async move {
                    // Snapshot capability data up front so the borrow on
                    // `caller` is released before we touch the awaited
                    // future.
                    let (caps, http, plugin_name) = {
                        let data = caller.data();
                        (
                            data.caps.clone(),
                            Arc::clone(&data.http),
                            data.plugin_name.clone(),
                        )
                    };

                    if !caps.fetch {
                        return Ok(FETCH_DISABLED);
                    }

                    let Some(Extern::Memory(mem)) = caller.get_export("memory") else {
                        return Err(wasmtime::Error::msg("plugin did not export `memory`"));
                    };

                    // Read URL + optional body out of plugin memory.
                    let (url_str, body_bytes) = {
                        let data = mem.data(&caller);
                        let url = read_string(data, url_ptr, url_len)?;
                        let body = if body_len > 0 {
                            read_bytes(data, body_ptr, body_len)?.to_vec()
                        } else {
                            Vec::new()
                        };
                        (url, body)
                    };

                    let parsed = match url::Url::parse(&url_str) {
                        Ok(u) => u,
                        Err(e) => {
                            warn!(target: "brarr_plugin_host", plugin = %plugin_name, error = %e, "host_fetch: invalid URL");
                            return Ok(FETCH_TRANSPORT_ERROR);
                        }
                    };
                    let host = match parsed.host_str() {
                        Some(h) => h.to_string(),
                        None => return Ok(FETCH_TRANSPORT_ERROR),
                    };
                    if !caps.allows_host(&host) {
                        warn!(
                            target: "brarr_plugin_host",
                            plugin = %plugin_name,
                            %host,
                            "host_fetch: host not in allowlist"
                        );
                        return Ok(FETCH_HOST_BLOCKED);
                    }

                    let Ok(req) = build_request(&http, method, parsed, body_bytes) else {
                        return Ok(FETCH_TRANSPORT_ERROR);
                    };
                    let req = req.timeout(caps.fetch_timeout);

                    debug!(
                        target: "brarr_plugin_host",
                        plugin = %plugin_name,
                        method,
                        %host,
                        "host_fetch dispatch"
                    );

                    let response = match req.send().await {
                        Ok(r) => r,
                        Err(e) => {
                            warn!(target: "brarr_plugin_host", plugin = %plugin_name, error = %e, "host_fetch transport");
                            return Ok(FETCH_TRANSPORT_ERROR);
                        }
                    };
                    let status = i32::from(response.status().as_u16());
                    let body = match response.bytes().await {
                        Ok(b) => b,
                        Err(e) => {
                            warn!(target: "brarr_plugin_host", plugin = %plugin_name, error = %e, "host_fetch body");
                            return Ok(FETCH_TRANSPORT_ERROR);
                        }
                    };

                    write_response_into_plugin(&mut caller, mem, &body, out_handle).await?;
                    Ok(status)
                })
            },
        )
        .map_err(PluginError::from)?;
    Ok(())
}

const FETCH_TRANSPORT_ERROR: i32 = -1;
const FETCH_DISABLED: i32 = -2;
const FETCH_HOST_BLOCKED: i32 = -3;

fn build_request(
    http: &reqwest::Client,
    method: i32,
    url: url::Url,
    body: Vec<u8>,
) -> Result<reqwest::RequestBuilder, ()> {
    let m = match method {
        0 => reqwest::Method::GET,
        1 => reqwest::Method::POST,
        2 => reqwest::Method::PUT,
        3 => reqwest::Method::DELETE,
        _ => return Err(()),
    };
    let mut req = http.request(m, url);
    if !body.is_empty() {
        req = req.body(body);
    }
    Ok(req)
}

async fn write_response_into_plugin(
    caller: &mut Caller<'_, HostState>,
    mem: Memory,
    body: &[u8],
    out_handle: i32,
) -> Result<(), wasmtime::Error> {
    // Ask the plugin to allocate space for the body using its own
    // allocator so the bytes live in a region the plugin can free.
    let alloc: TypedFunc<i32, i32> = caller
        .get_export("plugin_alloc")
        .and_then(Extern::into_func)
        .ok_or_else(|| wasmtime::Error::msg("plugin missing plugin_alloc"))?
        .typed(&*caller)?;
    let len_i32 = i32::try_from(body.len())
        .map_err(|_| wasmtime::Error::msg("response body too large for plugin allocator"))?;
    let ptr = alloc.call_async(caller.as_context_mut(), len_i32).await?;

    // Write body bytes at `ptr`.
    let mem_view = mem.data_mut(&mut *caller);
    let ptr_usize =
        usize::try_from(ptr).map_err(|_| wasmtime::Error::msg("plugin_alloc returned negative"))?;
    let end = ptr_usize
        .checked_add(body.len())
        .ok_or_else(|| wasmtime::Error::msg("response ptr+len overflows"))?;
    let dest = mem_view
        .get_mut(ptr_usize..end)
        .ok_or_else(|| wasmtime::Error::msg("response slice OOB in plugin memory"))?;
    dest.copy_from_slice(body);

    // Write (ptr, len) into out_handle (two little-endian u32s).
    let out_usize =
        usize::try_from(out_handle).map_err(|_| wasmtime::Error::msg("out_handle negative"))?;
    let out_end = out_usize
        .checked_add(8)
        .ok_or_else(|| wasmtime::Error::msg("out_handle+8 overflows"))?;
    let handle_slice = mem_view
        .get_mut(out_usize..out_end)
        .ok_or_else(|| wasmtime::Error::msg("out_handle slice OOB"))?;
    let ptr_le = u32::try_from(ptr_usize).unwrap_or(u32::MAX).to_le_bytes();
    let body_len_le = u32::try_from(body.len()).unwrap_or(u32::MAX).to_le_bytes();
    handle_slice[..4].copy_from_slice(&ptr_le);
    handle_slice[4..].copy_from_slice(&body_len_le);
    Ok(())
}

fn read_string(data: &[u8], ptr: i32, len: i32) -> Result<String, wasmtime::Error> {
    let slice = read_bytes(data, ptr, len)?;
    std::str::from_utf8(slice)
        .map(str::to_owned)
        .map_err(|e| wasmtime::Error::msg(format!("invalid utf-8 from plugin: {e}")))
}

fn read_bytes(data: &[u8], ptr: i32, len: i32) -> Result<&[u8], wasmtime::Error> {
    let ptr_usize =
        usize::try_from(ptr).map_err(|_| wasmtime::Error::msg("negative ptr from plugin"))?;
    let len_usize =
        usize::try_from(len).map_err(|_| wasmtime::Error::msg("negative len from plugin"))?;
    let end = ptr_usize
        .checked_add(len_usize)
        .ok_or_else(|| wasmtime::Error::msg("ptr+len overflows usize"))?;
    data.get(ptr_usize..end)
        .ok_or_else(|| wasmtime::Error::msg("ptr/len out of bounds in plugin memory"))
}

fn emit_log(level: i32, plugin: &str, msg: &str) {
    match level {
        0 => event!(target: "brarr_plugin_host", Level::TRACE, plugin, msg),
        1 => event!(target: "brarr_plugin_host", Level::DEBUG, plugin, msg),
        3 => event!(target: "brarr_plugin_host", Level::WARN, plugin, msg),
        4 => event!(target: "brarr_plugin_host", Level::ERROR, plugin, msg),
        // Treat unknown / 2 (info) and any out-of-range value as INFO.
        _ => event!(target: "brarr_plugin_host", Level::INFO, plugin, msg),
    }
}
