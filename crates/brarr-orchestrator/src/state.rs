//! Shared application state.
//!
//! Both the Axum router and the tonic service hold an [`AppState`].
//! Internally it's `Arc<Inner>` so cloning is cheap and the borrow
//! checker is happy across async tasks.
//!
//! The state owns a single `wasmtime::Engine` that every WASM plugin
//! shares — Engine construction is the expensive part (cranelift
//! initialization), individual `Module::new` calls are comparatively
//! cheap. Plugin modules are *not* cached today: each search re-loads
//! every plugin tracker from disk. That's intentional simplicity for
//! the first integration cut; a future revision can add a cache keyed
//! on tracker id + plugin mtime.
//!
//! ## Hot-reload runtime config
//!
//! [`RuntimeConfig`] carries the knobs the operator can edit at run
//! time through the admin UI (`/settings`). Each field is wrapped in
//! [`arc_swap::ArcSwap`] so reads are lock-free and writes flip an
//! atomic pointer — no request stalls when the operator hits save.
//!
//! The `RuntimeConfig` defaults are equivalent to "no overrides":
//! `AuthConfig::Disabled`, empty bypass, no public URL,
//! `DEFAULT_POLL_INTERVAL`, no-op log reloader. The production
//! `main.rs` seeds it from env + DB; tests use the default.

#![allow(
    clippy::expect_used,
    reason = "wasmtime engine init failure is a host architecture problem worth crashing on"
)]

use std::sync::Arc;
use std::time::Duration;

use arc_swap::{ArcSwap, Guard};
use brarr_decision_service::Engine;
use brarr_plugin_host::{DEFAULT_TICK_INTERVAL, WasmEpochTicker};
use wasmtime::{Config, Engine as WasmEngine};

use crate::auth::{AuthConfig, BypassConfig};
use crate::db::Pool;

/// Default poller cadence echoed from [`crate::poll`] so the runtime
/// config has a sensible default when no override exists yet.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(1800);

/// Default history-retention window (days) echoed from
/// [`crate::maintenance`]. Keeps the `decisions` table bounded out of
/// the box; the operator can widen, narrow, or disable it (`0`) from
/// `/settings`.
const DEFAULT_RETENTION_DAYS: u32 = 7;

/// Hot-reloadable runtime configuration. Held inside the Arc'd
/// [`AppState`] inner; readers go through accessors on
/// [`AppState`] (`auth`, `bypass`, `public_url`, `poll_interval`).
pub struct RuntimeConfig {
    /// Admin token configuration.
    pub auth: ArcSwap<AuthConfig>,
    /// Trusted-peer allowlist + trusted-proxy list.
    pub bypass: ArcSwap<BypassConfig>,
    /// External base URL stamped into push proxy links. `None` falls
    /// back to the per-request `X-Forwarded-Host` derivation.
    pub public_url: ArcSwap<Option<String>>,
    /// *arr poller cadence.
    pub poll_interval: ArcSwap<Duration>,
    /// History-retention window in days (`0` = keep forever). Read by
    /// the background maintenance task on every cycle so edits from
    /// `/settings` take effect without a respawn.
    pub retention_days: ArcSwap<u32>,
    /// Closure that reloads the `tracing-subscriber` env filter at
    /// runtime. Defaults to a no-op so tests don't have to wire a
    /// subscriber.
    pub log_reload: LogReloader,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            auth: ArcSwap::from_pointee(AuthConfig::Disabled),
            bypass: ArcSwap::from_pointee(BypassConfig::default()),
            public_url: ArcSwap::from_pointee(None),
            poll_interval: ArcSwap::from_pointee(DEFAULT_POLL_INTERVAL),
            retention_days: ArcSwap::from_pointee(DEFAULT_RETENTION_DAYS),
            log_reload: LogReloader::noop(),
        }
    }
}

/// Boxed closure backing [`LogReloader`]. Aliased to satisfy
/// `clippy::type_complexity` without an inline `#[allow]`.
type LogReloadFn = dyn Fn(&str) -> Result<(), String> + Send + Sync;

/// Type-erased log-level reloader. Wraps the `tracing-subscriber`
/// reload handle so the settings UI can swap the env-filter spec at
/// runtime without restarting the process.
#[derive(Clone)]
pub struct LogReloader {
    f: Arc<LogReloadFn>,
}

impl LogReloader {
    /// Build a reloader from an arbitrary closure. Production binds
    /// this to `tracing_subscriber::reload::Handle::reload`; tests use
    /// [`Self::noop`] when they don't care about log filtering.
    pub fn new<F>(f: F) -> Self
    where
        F: Fn(&str) -> Result<(), String> + Send + Sync + 'static,
    {
        Self { f: Arc::new(f) }
    }

    /// No-op reloader for tests and dev-mode boots that didn't wire a
    /// real handle. `apply` always returns `Ok(())`.
    #[must_use]
    pub fn noop() -> Self {
        Self::new(|_| Ok(()))
    }

    /// Apply `spec` (a `tracing-subscriber` env-filter string) to the
    /// running subscriber.
    ///
    /// # Errors
    ///
    /// Returns a human-readable error message when the spec fails to
    /// parse or the reload handle has already been dropped.
    pub fn apply(&self, spec: &str) -> Result<(), String> {
        (self.f)(spec)
    }
}

impl std::fmt::Debug for LogReloader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogReloader").finish_non_exhaustive()
    }
}

/// Cheaply cloneable handle to shared orchestrator state.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    pool: Pool,
    engine: Engine,
    wasm_engine: WasmEngine,
    /// Background ticker advancing the wasm engine epoch so per-plugin
    /// deadlines actually fire. Lifetime tied to this `Inner`; the
    /// task aborts when the last `AppState` clone is dropped.
    wasm_ticker: WasmEpochTicker,
    runtime: RuntimeConfig,
}

impl AppState {
    /// Build a new state handle with auth disabled. Convenience for
    /// tests and dev mode.
    ///
    /// # Panics
    ///
    /// See [`Self::with_runtime`].
    #[must_use]
    pub fn new(pool: Pool, engine: Engine) -> Self {
        Self::with_runtime(pool, engine, RuntimeConfig::default())
    }

    /// Build a state handle with an explicit [`AuthConfig`] and
    /// default everything else. Compat shim — new code should pass a
    /// fully-populated [`RuntimeConfig`] via [`Self::with_runtime`].
    ///
    /// # Panics
    ///
    /// See [`Self::with_runtime`].
    #[must_use]
    pub fn with_auth(pool: Pool, engine: Engine, auth: AuthConfig) -> Self {
        Self::with_auth_and_bypass(pool, engine, auth, BypassConfig::default())
    }

    /// Build a state handle with both [`AuthConfig`] and a
    /// [`BypassConfig`] (trusted-peer allowlist + trusted-proxy list).
    /// Compat shim retained for the older integration tests.
    ///
    /// # Panics
    ///
    /// See [`Self::with_runtime`].
    #[must_use]
    pub fn with_auth_and_bypass(
        pool: Pool,
        engine: Engine,
        auth: AuthConfig,
        bypass: BypassConfig,
    ) -> Self {
        let runtime = RuntimeConfig {
            auth: ArcSwap::from_pointee(auth),
            bypass: ArcSwap::from_pointee(bypass),
            ..RuntimeConfig::default()
        };
        Self::with_runtime(pool, engine, runtime)
    }

    /// Build a state handle with a fully-populated [`RuntimeConfig`].
    /// Used by `main.rs` once it has merged DB settings on top of env.
    ///
    /// The wasm engine is created with `async_support(true)` so plugins
    /// can use the async host imports (`host_fetch`, etc.).
    ///
    /// # Panics
    ///
    /// Panics if `wasmtime::Engine::new` fails — that only happens on
    /// unsupported host architectures, which is a configuration error
    /// worth crashing on rather than papering over.
    #[must_use]
    pub fn with_runtime(pool: Pool, engine: Engine, runtime: RuntimeConfig) -> Self {
        let mut wasm_cfg = Config::new();
        wasm_cfg.async_support(true);
        wasm_cfg.epoch_interruption(true);
        let wasm_engine =
            WasmEngine::new(&wasm_cfg).expect("build async wasmtime engine on supported host");
        let wasm_ticker =
            WasmEpochTicker::spawn(&Arc::new(wasm_engine.clone()), DEFAULT_TICK_INTERVAL);
        Self {
            inner: Arc::new(Inner {
                pool,
                engine,
                wasm_engine,
                wasm_ticker,
                runtime,
            }),
        }
    }

    /// Borrow the shared epoch ticker. Used by [`crate::search`] to
    /// compute per-plugin deadline ticks.
    #[must_use]
    pub fn wasm_ticker(&self) -> &WasmEpochTicker {
        &self.inner.wasm_ticker
    }

    /// Snapshot the current auth configuration. The returned guard
    /// auto-derefs to [`AuthConfig`] for method calls; once it drops,
    /// the runtime is free to swap the pointer.
    #[must_use]
    pub fn auth(&self) -> Guard<Arc<AuthConfig>> {
        self.inner.runtime.auth.load()
    }

    /// Snapshot the current bypass configuration.
    #[must_use]
    pub fn bypass(&self) -> Guard<Arc<BypassConfig>> {
        self.inner.runtime.bypass.load()
    }

    /// Convenience: snapshot just the token as an owned `String`.
    /// Used by call sites that need the value to outlive the lock
    /// guard (e.g. building a payload after dropping the borrow).
    #[must_use]
    pub fn auth_token_owned(&self) -> Option<String> {
        self.inner.runtime.auth.load().token().map(str::to_string)
    }

    /// Cloned snapshot of the current public-URL override. `None` →
    /// fall back to the request-derived base URL.
    #[must_use]
    pub fn public_url(&self) -> Option<String> {
        self.inner.runtime.public_url.load().as_ref().clone()
    }

    /// Current poller cadence. `Duration` is `Copy` so a snapshot is
    /// cheap and locks are unnecessary on the read path.
    #[must_use]
    pub fn poll_interval(&self) -> Duration {
        **self.inner.runtime.poll_interval.load()
    }

    /// Current history-retention window in days (`0` = keep forever).
    /// Read by [`crate::maintenance`] each cycle so `/settings` edits
    /// apply without a respawn.
    #[must_use]
    pub fn retention_days(&self) -> u32 {
        **self.inner.runtime.retention_days.load()
    }

    /// Borrow the full runtime config so writers (the `/settings`
    /// handler) can swap individual fields directly.
    #[must_use]
    pub fn runtime(&self) -> &RuntimeConfig {
        &self.inner.runtime
    }

    /// Borrow the connection pool.
    #[must_use]
    pub fn pool(&self) -> &Pool {
        &self.inner.pool
    }

    /// Borrow the rules engine.
    #[must_use]
    pub fn engine(&self) -> &Engine {
        &self.inner.engine
    }

    /// Borrow the shared `wasmtime` engine. Used by [`crate::search`]
    /// to instantiate plugin trackers without paying the cranelift
    /// init cost per call.
    #[must_use]
    pub fn wasm_engine(&self) -> &WasmEngine {
        &self.inner.wasm_engine
    }
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("pool", &"<sqlx::SqlitePool>")
            .field("engine", &"<brarr_decision_service::Engine>")
            .field("wasm_engine", &"<wasmtime::Engine>")
            .finish()
    }
}
