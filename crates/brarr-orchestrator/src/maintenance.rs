//! Background database-maintenance task.
//!
//! Mirrors [`crate::poll`]: a fire-and-forget tokio task that wakes on a
//! fixed cadence and prunes `decisions` / `searches` history older than
//! the operator-configured retention window, then reclaims the freed
//! space. The window is read from [`crate::AppState::retention_days`] on
//! every cycle, so edits from `/settings` take effect on the next tick —
//! no respawn required (same hot-reload contract as the poller).
//!
//! The actual SQL lives in [`crate::db::maintenance`]; this module only
//! owns the scheduling loop and logging.

use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio::time;
use tracing::{info, warn};

use crate::AppState;
use crate::db::maintenance::{self, MaintenanceOutcome};

/// How often the maintenance task wakes. 6h is frequent enough to keep
/// the `decisions` table from ballooning between runs, infrequent enough
/// that the `wal_checkpoint` / `incremental_vacuum` overhead is noise.
pub const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(6 * 3600);

/// Delay before the first prune so it doesn't pile onto the startup
/// burst (migrations, poller's immediate first cycle, server binds).
const STARTUP_DELAY: Duration = Duration::from_secs(60);

/// Spawn the background maintenance task. Returns the [`JoinHandle`] so
/// the caller can keep it alive — dropping the handle aborts the task.
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    let state = Arc::new(state);
    info!(
        target: "brarr_orchestrator::maintenance",
        retention_days = state.retention_days(),
        interval_secs = MAINTENANCE_INTERVAL.as_secs(),
        "starting db maintenance task (retention is hot-reloadable via /settings)"
    );
    tokio::spawn(async move {
        time::sleep(STARTUP_DELAY).await;
        loop {
            run_one_cycle(&state).await;
            time::sleep(MAINTENANCE_INTERVAL).await;
        }
    })
}

/// Run one prune + reclaim pass. Errors are logged, never propagated —
/// a transient DB hiccup must not kill the long-lived task.
async fn run_one_cycle(state: &AppState) {
    let retention_days = state.retention_days();
    if retention_days == 0 {
        info!(
            target: "brarr_orchestrator::maintenance",
            "retention disabled (0 days); skipping prune"
        );
        return;
    }
    match maintenance::run_prune(state.pool(), retention_days).await {
        Ok(outcome) => {
            log_outcome(retention_days, outcome);
            if !outcome.is_empty() {
                reclaim(state).await;
            }
        }
        Err(e) => warn!(
            target: "brarr_orchestrator::maintenance",
            error = %e,
            "prune cycle failed"
        ),
    }
}

/// Truncate the WAL and hand freed pages back to the OS after a prune
/// that actually deleted rows.
async fn reclaim(state: &AppState) {
    if let Err(e) = maintenance::checkpoint_wal(state.pool()).await {
        warn!(target: "brarr_orchestrator::maintenance", error = %e, "wal checkpoint failed");
    }
    if let Err(e) = maintenance::incremental_vacuum(state.pool()).await {
        warn!(target: "brarr_orchestrator::maintenance", error = %e, "incremental vacuum failed");
    }
}

fn log_outcome(retention_days: u32, outcome: MaintenanceOutcome) {
    info!(
        target: "brarr_orchestrator::maintenance",
        retention_days,
        decisions_deleted = outcome.decisions_deleted,
        searches_deleted = outcome.searches_deleted,
        "maintenance cycle complete"
    );
}
