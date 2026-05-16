//! Plugin loader + [`brarr_core::TrackerProvider`] adapter.
//!
//! The host runs wasmtime in **async mode** ([`Config::async_support`])
//! so host imports like `host_fetch` can `.await` real network I/O
//! without blocking a runtime worker. Every `TypedFunc` invocation
//! therefore goes through `call_async`; the per-instance `Store` is
//! protected by a `tokio::sync::Mutex` (cannot use `std::sync::Mutex`
//! because the critical section spans `.await` points).

use std::path::Path;
use std::sync::Arc;

use brarr_core::{ProviderError, ProviderFuture, Release, TmdbId, TrackerProvider, TrackerSource};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};
use wasmtime::{Config, Engine, Instance, Linker, Module, Store, TypedFunc};

use crate::dto::{self, PluginRelease};
use crate::error::{PluginError, PluginResult};
use crate::host::{HostCapabilities, HostState, MemoryLimiter, install_imports};
use crate::ticker::{DEFAULT_TICK_INTERVAL, WasmEpochTicker};

/// ABI version this host build implements.
pub const SUPPORTED_ABI_VERSION: i32 = 1;

/// Configuration knobs for a plugin instance.
#[derive(Debug, Clone)]
pub struct PluginConfig {
    /// Tracker identity the host stamps onto every release the plugin
    /// returns. Plugins cannot influence this — keeps them from
    /// impersonating a different tracker.
    pub tracker: TrackerSource,
    /// Capability gating.
    pub capabilities: HostCapabilities,
    /// Optional pre-built HTTP client. When `None`, the loader builds a
    /// default `reqwest::Client` per plugin. Sharing one client across
    /// many plugins keeps the connection pool warm.
    pub http: Option<Arc<reqwest::Client>>,
}

impl PluginConfig {
    /// Build a config with default capabilities (logging enabled,
    /// fetch disabled).
    #[must_use]
    pub fn new(tracker: TrackerSource) -> Self {
        Self {
            tracker,
            capabilities: HostCapabilities::default(),
            http: None,
        }
    }

    /// Override capabilities.
    #[must_use]
    pub fn with_capabilities(mut self, caps: HostCapabilities) -> Self {
        self.capabilities = caps;
        self
    }

    /// Override the HTTP client. Useful in tests (point at wiremock).
    #[must_use]
    pub fn with_http(mut self, http: Arc<reqwest::Client>) -> Self {
        self.http = Some(http);
        self
    }
}

/// A loaded plugin, ready to serve as a [`TrackerProvider`].
///
/// `Arc`-wrapped internally so cloning is cheap. The `Store` lives
/// behind a `tokio::sync::Mutex` because plugin calls span `.await`
/// points (host_fetch is async).
pub struct WasmTrackerProvider {
    inner: Arc<PluginInner>,
}

impl std::fmt::Debug for WasmTrackerProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmTrackerProvider")
            .field("plugin_name", &self.inner.plugin_name)
            .field("tracker", &self.inner.config.tracker.name)
            .finish()
    }
}

struct PluginInner {
    config: PluginConfig,
    plugin_name: String,
    /// Epoch deadline (in ticks) reset before each guest call so a
    /// long-running search times out independently rather than letting
    /// previous calls eat the budget.
    deadline_ticks: u64,
    /// Ticker the provider owns when constructed via `load_bytes` /
    /// `load_file`. Kept here so the background tokio task that
    /// advances the engine epoch survives as long as the provider.
    /// `None` when the caller supplied their own ticker via
    /// `load_with_engine` (orchestrator path).
    owned_ticker: Option<WasmEpochTicker>,
    runtime: Mutex<PluginRuntime>,
}

struct PluginRuntime {
    store: Store<HostState>,
    instance: Instance,
    alloc: TypedFunc<i32, i32>,
    free: TypedFunc<(i32, i32), ()>,
    search: TypedFunc<(i32, i32), i32>,
}

impl WasmTrackerProvider {
    /// Build an [`Engine`] suitable for use with the plugin host with
    /// `async_support` and `epoch_interruption` both enabled.
    ///
    /// # Errors
    ///
    /// Propagates any [`wasmtime::Error`] from `Engine::new`.
    pub fn async_engine() -> PluginResult<Engine> {
        let mut config = Config::new();
        config.async_support(true);
        config.epoch_interruption(true);
        Engine::new(&config).map_err(PluginError::from)
    }

    /// Compile + instantiate a plugin from a .wasm file (or .wat — the
    /// wasmtime `wat` feature transparently accepts both).
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::Io`] if the file cannot be read,
    /// [`PluginError::Wasm`] if compilation or instantiation fails,
    /// [`PluginError::MissingExport`] if any required symbol is absent,
    /// or [`PluginError::UnsupportedAbi`] if the plugin reports a
    /// version this host does not implement.
    pub async fn load_file(path: &Path, config: PluginConfig) -> PluginResult<Self> {
        let bytes = std::fs::read(path)?;
        Self::load_bytes(&bytes, config).await
    }

    /// Compile + instantiate a plugin from a raw byte slice. Builds an
    /// owned engine and ticker for this single plugin; callers that
    /// load many plugins should pre-build both and use
    /// [`Self::load_with_engine`] to share them.
    ///
    /// # Errors
    ///
    /// See [`Self::load_file`].
    pub async fn load_bytes(bytes: &[u8], config: PluginConfig) -> PluginResult<Self> {
        let engine = Self::async_engine()?;
        let ticker = WasmEpochTicker::spawn(&Arc::new(engine.clone()), DEFAULT_TICK_INTERVAL);
        let mut provider = Self::load_with_engine(&engine, &ticker, bytes, config).await?;
        // Hand ownership of the ticker to the provider so the background
        // task survives past this function. Safe because `inner` was
        // just created and there are no other Arc clones yet.
        if let Some(inner) = Arc::get_mut(&mut provider.inner) {
            inner.owned_ticker = Some(ticker);
        }
        Ok(provider)
    }

    /// Like [`Self::load_bytes`] but with a caller-supplied engine and
    /// ticker. **The engine must have been built with
    /// `async_support(true)` and `epoch_interruption(true)`** — see
    /// [`Self::async_engine`]. The ticker must be advancing the same
    /// engine, otherwise the per-store deadline never fires.
    ///
    /// The caller owns the ticker; this loader does **not** keep it
    /// alive. Pair with [`Self::load_bytes`] when you want a one-shot
    /// plugin that manages its own ticker.
    ///
    /// # Errors
    ///
    /// See [`Self::load_file`].
    pub async fn load_with_engine(
        engine: &Engine,
        ticker: &WasmEpochTicker,
        bytes: &[u8],
        config: PluginConfig,
    ) -> PluginResult<Self> {
        let module = Module::new(engine, bytes)?;

        // Probe the name up-front using a temporary host state — we
        // need the name to populate the real host state.
        let probe_name = probe_plugin_name(engine, ticker, &module, &config.capabilities).await?;
        debug!(target: "brarr_plugin_host", plugin = %probe_name, "instantiating plugin");

        let http = config
            .http
            .clone()
            .unwrap_or_else(|| Arc::new(reqwest::Client::new()));

        let mut store = Store::new(
            engine,
            HostState {
                plugin_name: probe_name.clone(),
                caps: config.capabilities.clone(),
                http: Arc::clone(&http),
                limiter: MemoryLimiter {
                    max_pages: config.capabilities.max_memory_pages,
                },
            },
        );
        store.limiter(|state| &mut state.limiter);
        let deadline_ticks = ticker.ticks_for(config.capabilities.call_deadline);
        store.set_epoch_deadline(deadline_ticks);
        let mut linker: Linker<HostState> = Linker::new(engine);
        install_imports(&mut linker)?;
        let instance = linker.instantiate_async(&mut store, &module).await?;

        check_abi_version(&mut store, &instance).await?;

        let alloc = typed_export::<i32, i32>(&mut store, &instance, "plugin_alloc")?;
        let free = typed_export::<(i32, i32), ()>(&mut store, &instance, "plugin_free")?;
        let search =
            typed_export::<(i32, i32), i32>(&mut store, &instance, "plugin_search_by_tmdb")?;

        info!(
            target: "brarr_plugin_host",
            plugin = %probe_name,
            tracker = %config.tracker.name,
            "plugin loaded"
        );

        Ok(Self {
            inner: Arc::new(PluginInner {
                config,
                plugin_name: probe_name,
                deadline_ticks,
                owned_ticker: None,
                runtime: Mutex::new(PluginRuntime {
                    store,
                    instance,
                    alloc,
                    free,
                    search,
                }),
            }),
        })
    }

    /// Human-readable plugin name as reported by `plugin_name`.
    #[must_use]
    pub fn plugin_name(&self) -> &str {
        &self.inner.plugin_name
    }

    async fn search_inner(&self, tmdb: TmdbId) -> PluginResult<Vec<Release>> {
        let mut guard = self.inner.runtime.lock().await;
        let runtime = &mut *guard;

        // Fresh deadline per call: relative to the engine's *current*
        // epoch, so the ticker advancing in the background actually
        // moves the trap point forward.
        runtime.store.set_epoch_deadline(self.inner.deadline_ticks);

        // Allocate an 8-byte region in the plugin to receive (ptr, len).
        let out_handle = runtime.alloc.call_async(&mut runtime.store, 8).await?;

        let tmdb_i32 = i32::try_from(tmdb.get())
            .map_err(|_| PluginError::BadOutput(format!("tmdb {} > i32::MAX", tmdb.get())))?;
        let rc = runtime
            .search
            .call_async(&mut runtime.store, (tmdb_i32, out_handle))
            .await?;
        if rc != 0 {
            let _ = runtime
                .free
                .call_async(&mut runtime.store, (out_handle, 8))
                .await;
            return Err(PluginError::PluginCode(rc));
        }

        // Read (ptr, len) from out_handle in plugin memory.
        let memory = runtime
            .instance
            .get_memory(&mut runtime.store, "memory")
            .ok_or(PluginError::MissingExport {
                name: "memory",
                signature: "(memory)",
            })?;
        let data = memory.data(&runtime.store);
        let out_idx = usize_from(out_handle)?;
        let handle_slice = data
            .get(out_idx..out_idx + 8)
            .ok_or_else(|| PluginError::BadOutput("out_handle slice OOB".into()))?;
        let ptr_le = [
            handle_slice[0],
            handle_slice[1],
            handle_slice[2],
            handle_slice[3],
        ];
        let len_le = [
            handle_slice[4],
            handle_slice[5],
            handle_slice[6],
            handle_slice[7],
        ];
        let ptr = u32::from_le_bytes(ptr_le) as usize;
        let len = u32::from_le_bytes(len_le) as usize;

        let json_bytes = data
            .get(ptr..ptr + len)
            .ok_or_else(|| PluginError::BadOutput("response slice OOB".into()))?
            .to_vec();

        // Free the response region and the handle.
        let _ = runtime
            .free
            .call_async(&mut runtime.store, (i32_from(ptr)?, i32_from(len)?))
            .await;
        let _ = runtime
            .free
            .call_async(&mut runtime.store, (out_handle, 8))
            .await;

        let plugin_releases: Vec<PluginRelease> = serde_json::from_slice(&json_bytes)
            .map_err(|e| PluginError::BadOutput(format!("response JSON decode failed: {e}")))?;

        let mut releases = Vec::with_capacity(plugin_releases.len());
        for pr in &plugin_releases {
            match dto::to_release(pr, self.inner.config.tracker.clone()) {
                Ok(r) => releases.push(r),
                Err(e) => {
                    warn!(
                        target: "brarr_plugin_host",
                        plugin = %self.inner.plugin_name,
                        id = %pr.id,
                        error = %e,
                        "skipping plugin release with invalid invariants"
                    );
                }
            }
        }
        Ok(releases)
    }
}

impl Clone for WasmTrackerProvider {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl TrackerProvider for WasmTrackerProvider {
    fn name(&self) -> &str {
        &self.inner.config.tracker.name
    }

    fn search_by_tmdb(
        &self,
        tmdb: TmdbId,
    ) -> ProviderFuture<'_, Result<Vec<Release>, ProviderError>> {
        let plugin_name = self.inner.plugin_name.clone();
        Box::pin(async move {
            self.search_inner(tmdb)
                .await
                .map_err(|e| e.into_provider(&plugin_name))
        })
    }
}

/// Instantiate just enough of the module to read `plugin_name()` so the
/// host state can carry the correct name before the real instance is
/// created. The probe uses a throwaway `Store` + `Linker`.
async fn probe_plugin_name(
    engine: &Engine,
    ticker: &WasmEpochTicker,
    module: &Module,
    caps: &HostCapabilities,
) -> PluginResult<String> {
    let limiter = MemoryLimiter {
        max_pages: caps.max_memory_pages,
    };
    let mut store = Store::new(
        engine,
        HostState {
            plugin_name: "<probe>".into(),
            caps: caps.clone(),
            http: Arc::new(reqwest::Client::new()),
            limiter,
        },
    );
    store.limiter(|state| &mut state.limiter);
    store.set_epoch_deadline(ticker.ticks_for(caps.call_deadline));
    let mut linker: Linker<HostState> = Linker::new(engine);
    install_imports(&mut linker)?;
    let instance = linker.instantiate_async(&mut store, module).await?;
    let name_fn = typed_export::<(), i64>(&mut store, &instance, "plugin_name")?;
    let packed = name_fn.call_async(&mut store, ()).await?;
    let (ptr, len) = unpack_ptr_len(packed);
    let memory = instance
        .get_memory(&mut store, "memory")
        .ok_or(PluginError::MissingExport {
            name: "memory",
            signature: "(memory)",
        })?;
    let data = memory.data(&store);
    let slice = data
        .get(ptr..ptr + len)
        .ok_or_else(|| PluginError::BadOutput("plugin_name slice OOB".into()))?;
    let name = std::str::from_utf8(slice)
        .map_err(|e| PluginError::BadOutput(format!("plugin_name not utf-8: {e}")))?;
    Ok(name.to_owned())
}

async fn check_abi_version(store: &mut Store<HostState>, instance: &Instance) -> PluginResult<()> {
    let f = typed_export::<(), i32>(store, instance, "plugin_abi_version")?;
    let got = f.call_async(&mut *store, ()).await?;
    if got == SUPPORTED_ABI_VERSION {
        Ok(())
    } else {
        Err(PluginError::UnsupportedAbi {
            got,
            supported: SUPPORTED_ABI_VERSION,
        })
    }
}

fn typed_export<Params, Results>(
    store: &mut Store<HostState>,
    instance: &Instance,
    name: &'static str,
) -> PluginResult<TypedFunc<Params, Results>>
where
    Params: wasmtime::WasmParams,
    Results: wasmtime::WasmResults,
{
    instance
        .get_typed_func::<Params, Results>(store, name)
        .map_err(|_| PluginError::MissingExport {
            name,
            signature: std::any::type_name::<fn(Params) -> Results>(),
        })
}

fn unpack_ptr_len(packed: i64) -> (usize, usize) {
    // Bit pattern is what matters; the i64→u64 reinterpretation is
    // intentional (plugin packs two u32s into the 64-bit return slot).
    let bits = u64::from_ne_bytes(packed.to_ne_bytes());
    let ptr = u32::try_from(bits & 0xFFFF_FFFF).unwrap_or(u32::MAX);
    let len = u32::try_from(bits >> 32).unwrap_or(u32::MAX);
    (ptr as usize, len as usize)
}

fn usize_from(v: i32) -> PluginResult<usize> {
    usize::try_from(v).map_err(|_| PluginError::BadOutput(format!("negative index {v}")))
}

fn i32_from(v: usize) -> PluginResult<i32> {
    i32::try_from(v).map_err(|_| PluginError::BadOutput(format!("index {v} > i32::MAX")))
}
