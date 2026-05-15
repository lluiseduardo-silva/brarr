//! Shared application state.
//!
//! Both the Axum router and the tonic service hold an [`AppState`].
//! Internally it's `Arc<Inner>` so cloning is cheap and the borrow
//! checker is happy across async tasks.

use std::sync::Arc;

use brarr_decision_service::Engine;

use crate::db::Pool;

/// Cheaply cloneable handle to shared orchestrator state.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    pool: Pool,
    engine: Engine,
}

impl AppState {
    /// Build a new state handle.
    #[must_use]
    pub fn new(pool: Pool, engine: Engine) -> Self {
        Self {
            inner: Arc::new(Inner { pool, engine }),
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
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("pool", &"<sqlx::SqlitePool>")
            .field("engine", &"<brarr_decision_service::Engine>")
            .finish()
    }
}
