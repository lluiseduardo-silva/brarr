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

use std::sync::Arc;

use brarr_decision_service::Engine;
use wasmtime::Engine as WasmEngine;

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
}

impl AppState {
    /// Build a new state handle.
    ///
    /// # Panics
    ///
    /// Panics only if `wasmtime::Engine::default()` fails (cranelift
    /// initialization). In practice that requires an unsupported host
    /// architecture and is treated as a configuration error worth
    /// crashing on rather than papering over.
    #[must_use]
    pub fn new(pool: Pool, engine: Engine) -> Self {
        Self {
            inner: Arc::new(Inner {
                pool,
                engine,
                wasm_engine: WasmEngine::default(),
            }),
        }
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
