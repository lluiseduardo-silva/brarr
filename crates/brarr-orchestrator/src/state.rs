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

#![allow(
    clippy::expect_used,
    reason = "wasmtime engine init failure is a host architecture problem worth crashing on"
)]

use std::sync::Arc;

use brarr_decision_service::Engine;
use brarr_plugin_host::{DEFAULT_TICK_INTERVAL, WasmEpochTicker};
use wasmtime::{Config, Engine as WasmEngine};

use crate::auth::{AuthConfig, BypassConfig};
use crate::db::Pool;

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
    auth: AuthConfig,
    bypass: Arc<BypassConfig>,
}

impl AppState {
    /// Build a new state handle with auth disabled. Convenience for
    /// tests and dev mode.
    ///
    /// # Panics
    ///
    /// See [`Self::with_auth`].
    #[must_use]
    pub fn new(pool: Pool, engine: Engine) -> Self {
        Self::with_auth(pool, engine, AuthConfig::Disabled)
    }

    /// Build a state handle with an explicit [`AuthConfig`].
    ///
    /// The wasm engine is created with `async_support(true)` so plugins
    /// can use the async host imports (`host_fetch`, etc.).
    ///
    /// Bypass defaults to empty — use [`Self::with_auth_and_bypass`] to
    /// wire the trusted-peer allowlist.
    ///
    /// # Panics
    ///
    /// Panics if `wasmtime::Engine::new` fails — that only happens on
    /// unsupported host architectures, which is a configuration error
    /// worth crashing on rather than papering over.
    #[must_use]
    pub fn with_auth(pool: Pool, engine: Engine, auth: AuthConfig) -> Self {
        Self::with_auth_and_bypass(pool, engine, auth, BypassConfig::default())
    }

    /// Build a state handle with both [`AuthConfig`] and a
    /// [`BypassConfig`] (trusted-peer allowlist + trusted-proxy list).
    ///
    /// # Panics
    ///
    /// Same as [`Self::with_auth`].
    #[must_use]
    pub fn with_auth_and_bypass(
        pool: Pool,
        engine: Engine,
        auth: AuthConfig,
        bypass: BypassConfig,
    ) -> Self {
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
                auth,
                bypass: Arc::new(bypass),
            }),
        }
    }

    /// Borrow the shared epoch ticker. Used by [`crate::search`] to
    /// compute per-plugin deadline ticks.
    #[must_use]
    pub fn wasm_ticker(&self) -> &WasmEpochTicker {
        &self.inner.wasm_ticker
    }

    /// Borrow the auth configuration.
    #[must_use]
    pub fn auth(&self) -> &AuthConfig {
        &self.inner.auth
    }

    /// Borrow the bypass configuration (trusted peers + trusted
    /// proxies). Always present — defaults to empty when not
    /// configured, which means no requests are bypassed.
    #[must_use]
    pub fn bypass(&self) -> &BypassConfig {
        &self.inner.bypass
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
