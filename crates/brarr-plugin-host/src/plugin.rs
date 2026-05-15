//! Plugin loader + [`brarr_core::TrackerProvider`] adapter.

use std::path::Path;
use std::sync::{Arc, Mutex};

use brarr_core::{ProviderError, ProviderFuture, Release, TmdbId, TrackerProvider, TrackerSource};
use tracing::{debug, info, warn};
use wasmtime::{Engine, Instance, Linker, Module, Store, TypedFunc};

use crate::dto::{self, PluginRelease};
use crate::error::{PluginError, PluginResult};
use crate::host::{HostCapabilities, HostState, install_imports};

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
}

impl PluginConfig {
    /// Build a config with default capabilities (logging enabled).
    #[must_use]
    pub fn new(tracker: TrackerSource) -> Self {
        Self {
            tracker,
            capabilities: HostCapabilities::default(),
        }
    }
}

/// A loaded plugin, ready to serve as a [`TrackerProvider`].
///
/// `Arc`-wrapped internally so cloning is cheap; the `Store` and
/// `Instance` live behind a `Mutex` because wasmtime's `Store` is not
/// `Sync` and a plugin instance is single-threaded by definition.
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
    /// `Store` + bound exports together so the entire plugin call
    /// happens under one mutex lock.
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
    pub fn load_file(path: &Path, config: PluginConfig) -> PluginResult<Self> {
        let bytes = std::fs::read(path)?;
        Self::load_bytes(&bytes, config)
    }

    /// Compile + instantiate a plugin from a raw byte slice.
    ///
    /// # Errors
    ///
    /// See [`Self::load_file`].
    pub fn load_bytes(bytes: &[u8], config: PluginConfig) -> PluginResult<Self> {
        let engine = Engine::default();
        Self::load_with_engine(&engine, bytes, config)
    }

    /// Like [`Self::load_bytes`] but with a caller-supplied engine —
    /// lets tests share one engine across many instantiations.
    ///
    /// # Errors
    ///
    /// See [`Self::load_file`].
    pub fn load_with_engine(
        engine: &Engine,
        bytes: &[u8],
        config: PluginConfig,
    ) -> PluginResult<Self> {
        let module = Module::new(engine, bytes)?;

        // Probe the name up-front using a temporary host state — we
        // need the name to populate the real host state.
        let probe_name = probe_plugin_name(engine, &module)?;
        debug!(target: "brarr_plugin_host", plugin = %probe_name, "instantiating plugin");

        let mut store = Store::new(
            engine,
            HostState {
                plugin_name: probe_name.clone(),
                caps: config.capabilities,
            },
        );
        let mut linker: Linker<HostState> = Linker::new(engine);
        install_imports(&mut linker)?;
        let instance = linker.instantiate(&mut store, &module)?;

        check_abi_version(&mut store, &instance)?;

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

    fn search_blocking(&self, tmdb: TmdbId) -> PluginResult<Vec<Release>> {
        let mut guard = self
            .inner
            .runtime
            .lock()
            .map_err(|_| PluginError::Wasm("plugin runtime mutex poisoned".into()))?;
        let runtime = &mut *guard;

        // Allocate an 8-byte region in the plugin to receive (ptr, len).
        let out_handle = runtime.alloc.call(&mut runtime.store, 8)?;

        let tmdb_i32 = i32::try_from(tmdb.get())
            .map_err(|_| PluginError::BadOutput(format!("tmdb {} > i32::MAX", tmdb.get())))?;
        let rc = runtime
            .search
            .call(&mut runtime.store, (tmdb_i32, out_handle))?;
        if rc != 0 {
            // Free the handle even on plugin error.
            let _ = runtime.free.call(&mut runtime.store, (out_handle, 8));
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
        let handle_slice = data
            .get(usize_from(out_handle)?..usize_from(out_handle)? + 8)
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
            .call(&mut runtime.store, (i32_from(ptr)?, i32_from(len)?));
        let _ = runtime.free.call(&mut runtime.store, (out_handle, 8));

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
        // Wasmtime calls are sync + CPU-bound; wrap in spawn_blocking to
        // not stall the async runtime.
        let me = self.clone();
        let plugin_name = self.inner.plugin_name.clone();
        Box::pin(async move {
            let join = tokio::task::spawn_blocking(move || me.search_blocking(tmdb)).await;
            match join {
                Ok(Ok(releases)) => Ok(releases),
                Ok(Err(e)) => Err(e.into_provider(&plugin_name)),
                Err(join_err) => Err(ProviderError::new(
                    plugin_name,
                    format!("plugin task join error: {join_err}"),
                )),
            }
        })
    }
}

/// Instantiate just enough of the module to read `plugin_name()` so the
/// host state can carry the correct name before the real instance is
/// created. The probe uses a throwaway `Store` + `Linker`.
fn probe_plugin_name(engine: &Engine, module: &Module) -> PluginResult<String> {
    let mut store = Store::new(
        engine,
        HostState {
            plugin_name: "<probe>".into(),
            caps: HostCapabilities::default(),
        },
    );
    let mut linker: Linker<HostState> = Linker::new(engine);
    install_imports(&mut linker)?;
    let instance = linker.instantiate(&mut store, module)?;
    let name_fn = typed_export::<(), i64>(&mut store, &instance, "plugin_name")?;
    let packed = name_fn.call(&mut store, ())?;
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

fn check_abi_version(store: &mut Store<HostState>, instance: &Instance) -> PluginResult<()> {
    let f = typed_export::<(), i32>(store, instance, "plugin_abi_version")?;
    let got = f.call(&mut *store, ())?;
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
