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
use wasmtime::{Config, Engine as WasmEngine};

use crate::auth::AuthConfig;
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
    auth: AuthConfig,
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
    /// # Panics
    ///
    /// Panics if `wasmtime::Engine::new` fails — that only happens on
    /// unsupported host architectures, which is a configuration error
    /// worth crashing on rather than papering over.
    #[must_use]
    pub fn with_auth(pool: Pool, engine: Engine, auth: AuthConfig) -> Self {
        let mut wasm_cfg = Config::new();
        wasm_cfg.async_support(true);
        let wasm_engine =
            WasmEngine::new(&wasm_cfg).expect("build async wasmtime engine on supported host");
        Self {
            inner: Arc::new(Inner {
                pool,
                engine,
                wasm_engine,
                auth,
            }),
        }
    }

    /// Borrow the auth configuration.
    #[must_use]
    pub fn auth(&self) -> &AuthConfig {
        &self.inner.auth
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
