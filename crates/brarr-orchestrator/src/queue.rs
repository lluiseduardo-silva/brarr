//! Following what the download clients are actually doing.
//!
//! [`crate::deliver`] leaves a grab at `sent` — handed over, and after
//! that brarr had no idea. This module closes the loop: it asks each
//! client how its downloads are going and walks the grab through
//! `sent → downloading → completed`, or `failed`.
//!
//! ## Progress is read live, not stored
//!
//! Only *state transitions* are persisted. Percentage, speed and ETA are
//! read from the client when the queue page renders and thrown away
//! after. Storing them would mean three more columns written every
//! minute for numbers that are stale the moment they land, and the queue
//! is a handful of rows — the client can answer for them on demand.
//!
//! ## "The client doesn't have it" is not the same as "I couldn't ask"
//!
//! A grab whose client says *no such download* has probably been removed
//! by hand, and after a grace period brarr marks it failed so the
//! scanner can look for something else. A grab whose client is
//! unreachable, disabled or deleted says nothing about the download —
//! failing it there would abandon a perfectly healthy download because a
//! container was restarting. [`Probe`] keeps the two apart.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use brarr_download_client::{DownloadClient, DownloadState, DownloadStatus};
use time::OffsetDateTime;
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::db::download_clients::{self, DownloadClientRow};
use crate::db::grabs::{self, Grab, GrabStatus};
use crate::{AppError, AppState};

/// How often the background sync runs. Downloads move fast enough that a
/// minute is the difference between a useful queue page and a stale one,
/// and cheap enough that it is a handful of local HTTP calls.
pub const SYNC_INTERVAL: Duration = Duration::from_secs(60);

/// How long a grab may go unrecognised by its own client before brarr
/// concludes it is gone.
///
/// Not zero, because both clients take a moment to list something they
/// just accepted — Radarr polls ten times before believing an add — and
/// a restarting client lists nothing at all for a while.
pub const MISSING_GRACE: Duration = Duration::from_secs(10 * 60);

/// Delay before the first sync so it doesn't pile onto the startup burst.
const STARTUP_DELAY: Duration = Duration::from_secs(45);

/// What one client said about one grab.
#[derive(Debug, Clone)]
pub enum Probe {
    /// The client knows this download and reported on it.
    Known(DownloadStatus),
    /// The client answered, and has no such download.
    Unknown,
    /// Nobody could be asked: the client row is gone, disabled, or the
    /// call failed. Says nothing about the download itself.
    Unreachable(String),
}

/// One row of the queue: the acquisition record plus whatever its client
/// had to say about it.
#[derive(Debug, Clone)]
pub struct QueueEntry {
    /// The persisted grab.
    pub grab: Grab,
    /// Display name of the client holding it, when there is one.
    pub client_name: Option<String>,
    /// Live reading.
    pub probe: Probe,
}

/// Counts from one sync pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncSummary {
    /// Grabs looked at.
    pub checked: usize,
    /// Grabs whose status changed.
    pub advanced: usize,
    /// Grabs marked failed this pass.
    pub failed: usize,
    /// Grabs whose client could not be asked.
    pub unreachable: usize,
}

/// Spawn the background sync. Same fire-and-forget contract as the
/// poller, the janitor and the scanner.
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    let state = Arc::new(state);
    info!(
        target: "brarr_orchestrator::queue",
        interval_secs = SYNC_INTERVAL.as_secs(),
        "starting the download-queue sync"
    );
    tokio::spawn(async move {
        sleep(STARTUP_DELAY).await;
        loop {
            match sync_once(&state).await {
                Ok(summary) if summary.checked > 0 => info!(
                    target: "brarr_orchestrator::queue",
                    checked = summary.checked,
                    advanced = summary.advanced,
                    failed = summary.failed,
                    unreachable = summary.unreachable,
                    "queue sync complete"
                ),
                Ok(_) => debug!(target: "brarr_orchestrator::queue", "queue empty"),
                Err(e) => {
                    warn!(target: "brarr_orchestrator::queue", error = %e, "queue sync failed");
                }
            }
            sleep(SYNC_INTERVAL).await;
        }
    })
}

/// Ask every client about every in-flight grab.
///
/// # Errors
///
/// Returns [`AppError::Database`] when the queue or the client list
/// cannot be read. A client that fails to answer becomes
/// [`Probe::Unreachable`], not an error.
pub async fn snapshot(state: &AppState) -> Result<Vec<QueueEntry>, AppError> {
    let grabs = grabs::queue(state.pool()).await?;
    if grabs.is_empty() {
        return Ok(Vec::new());
    }
    let clients: HashMap<Uuid, DownloadClientRow> = download_clients::list_all(state.pool())
        .await?
        .into_iter()
        .map(|row| (row.id, row))
        .collect();

    // One built client per row, not per grab: building one costs a
    // `reqwest::Client`, and qBittorrent's session lives inside it — so
    // reusing it also means one login for the whole pass.
    let mut built: HashMap<Uuid, Option<Box<dyn DownloadClient>>> = HashMap::new();
    let mut entries = Vec::with_capacity(grabs.len());
    for grab in grabs {
        let row = grab.client_id.and_then(|id| clients.get(&id));
        let probe = match (row, grab.client_item_id.as_deref()) {
            (Some(row), Some(item_id)) => {
                let client = built.entry(row.id).or_insert_with(|| {
                    match brarr_download_client::build(row.to_config()) {
                        Ok(c) => Some(c),
                        Err(e) => {
                            warn!(
                                target: "brarr_orchestrator::queue",
                                client = %row.name,
                                error = %e,
                                "could not build the download client"
                            );
                            None
                        }
                    }
                });
                match client {
                    Some(client) => match client.status(item_id).await {
                        Ok(Some(status)) => Probe::Known(status),
                        Ok(None) => Probe::Unknown,
                        Err(e) => Probe::Unreachable(format!("{e}")),
                    },
                    None => Probe::Unreachable("configuração do cliente inválida".to_owned()),
                }
            }
            // Handed over but never identified — a base32 magnet, say.
            (Some(_), None) => {
                Probe::Unreachable("a release não tem identificador no cliente".to_owned())
            }
            // Still reserved, or its client was deleted.
            (None, _) => Probe::Unreachable("sem cliente associado".to_owned()),
        };
        entries.push(QueueEntry {
            client_name: row.map(|r| r.name.clone()),
            grab,
            probe,
        });
    }
    Ok(entries)
}

/// One sync pass: read the queue and apply whatever the clients report.
///
/// # Errors
///
/// Returns [`AppError::Database`] on a DB failure.
pub async fn sync_once(state: &AppState) -> Result<SyncSummary, AppError> {
    let entries = snapshot(state).await?;
    let mut summary = SyncSummary::default();
    let now = OffsetDateTime::now_utc();
    for entry in entries {
        summary.checked += 1;
        let Some(next) = next_status(&entry, now) else {
            if matches!(entry.probe, Probe::Unreachable(_)) {
                summary.unreachable += 1;
            }
            continue;
        };
        if next.0 == entry.grab.status {
            continue;
        }
        grabs::set_status(state.pool(), entry.grab.id, next.0, next.1.as_deref()).await?;
        summary.advanced += 1;
        if next.0 == GrabStatus::Failed {
            summary.failed += 1;
        }
        info!(
            target: "brarr_orchestrator::queue",
            grab_id = %entry.grab.id,
            release = %entry.grab.release_name,
            from = entry.grab.status.label(),
            to = next.0.label(),
            "grab advanced"
        );
    }
    Ok(summary)
}

/// The status a grab should move to, and why. `None` leaves it alone.
///
/// Split out from the IO so the decision table is testable without a
/// client or a pool.
fn next_status(entry: &QueueEntry, now: OffsetDateTime) -> Option<(GrabStatus, Option<String>)> {
    match &entry.probe {
        Probe::Known(status) => match status.state {
            DownloadState::Queued | DownloadState::Downloading => {
                Some((GrabStatus::Downloading, None))
            }
            DownloadState::Completed => Some((GrabStatus::Completed, None)),
            DownloadState::Failed => Some((
                GrabStatus::Failed,
                Some(
                    status
                        .detail
                        .clone()
                        .unwrap_or_else(|| "o cliente reportou falha".to_owned()),
                ),
            )),
        },
        Probe::Unknown => {
            // A reservation that never reached a client is the delivery
            // path's business, not this one's.
            if entry.grab.status == GrabStatus::Reserved {
                return None;
            }
            // A finished download the client no longer lists is not a
            // loss: it downloaded, and the file is on disk waiting for
            // the importer. The operator clearing a completed torrent
            // out of qBittorrent is normal housekeeping.
            //
            // This matters most for a grab the import is *waiting* on:
            // waiting does not move `updated_at`, so the grace period
            // is already spent by the time the client forgets the
            // download. Failing it here would drop it out of
            // `blocks_search`, and the scanner would go buy the same
            // episode again — the exact churn this release removes.
            if entry.grab.status == GrabStatus::Completed {
                return None;
            }
            let age = now - entry.grab.updated_at;
            if age >= MISSING_GRACE {
                Some((
                    GrabStatus::Failed,
                    Some("o download sumiu do cliente".to_owned()),
                ))
            } else {
                // Too soon to conclude anything — see the module docs.
                None
            }
        }
        Probe::Unreachable(_) => None,
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert on happy paths"
)]
mod tests {
    use super::*;
    use crate::db::grabs::Protocol;

    fn grab(status: GrabStatus, updated_minutes_ago: i64) -> Grab {
        let now = OffsetDateTime::now_utc();
        Grab {
            id: Uuid::new_v4(),
            item_id: Uuid::new_v4(),
            episode_id: None,
            season_number: None,
            decision_id: None,
            provider_id: None,
            provider_name: "capybara".to_owned(),
            release_id_remote: "abc".to_owned(),
            release_name: "Matrix.1999.1080p".to_owned(),
            download_url: None,
            protocol: Protocol::Torrent,
            client_id: Some(Uuid::new_v4()),
            client_item_id: Some("hash".to_owned()),
            status,
            error: None,
            imported_path: None,
            file_missing_at: None,
            import_wait_reason: None,
            import_attempted_at: None,
            grabbed_at: now,
            updated_at: now - Duration::from_secs((updated_minutes_ago * 60).unsigned_abs()),
        }
    }

    fn entry(status: GrabStatus, probe: Probe, updated_minutes_ago: i64) -> QueueEntry {
        QueueEntry {
            grab: grab(status, updated_minutes_ago),
            client_name: Some("qb".to_owned()),
            probe,
        }
    }

    fn status(state: DownloadState) -> DownloadStatus {
        DownloadStatus {
            state,
            progress: 0.5,
            size_bytes: None,
            speed_bytes: None,
            eta_seconds: None,
            save_path: None,
            detail: None,
        }
    }

    #[test]
    fn a_running_download_moves_the_grab_to_downloading() {
        let e = entry(
            GrabStatus::Sent,
            Probe::Known(status(DownloadState::Downloading)),
            0,
        );
        let (next, _) = next_status(&e, OffsetDateTime::now_utc()).unwrap();
        assert_eq!(next, GrabStatus::Downloading);
    }

    #[test]
    fn a_finished_download_completes_the_grab() {
        let e = entry(
            GrabStatus::Downloading,
            Probe::Known(status(DownloadState::Completed)),
            0,
        );
        let (next, _) = next_status(&e, OffsetDateTime::now_utc()).unwrap();
        assert_eq!(next, GrabStatus::Completed);
    }

    #[test]
    fn a_client_side_failure_carries_its_reason_across() {
        let mut failed = status(DownloadState::Failed);
        failed.detail = Some("Unpacking failed".to_owned());
        let e = entry(GrabStatus::Downloading, Probe::Known(failed), 0);
        let (next, reason) = next_status(&e, OffsetDateTime::now_utc()).unwrap();
        assert_eq!(next, GrabStatus::Failed);
        assert_eq!(reason.as_deref(), Some("Unpacking failed"));
    }

    #[test]
    fn an_unreachable_client_never_touches_the_grab() {
        // The whole point of the distinction: a restarting container
        // must not abandon a healthy download.
        let e = entry(
            GrabStatus::Downloading,
            Probe::Unreachable("connection refused".to_owned()),
            600,
        );
        assert!(next_status(&e, OffsetDateTime::now_utc()).is_none());
    }

    #[test]
    fn a_download_the_client_does_not_know_gets_a_grace_period() {
        // Both clients take a moment to list something just accepted.
        let fresh = entry(GrabStatus::Sent, Probe::Unknown, 1);
        assert!(next_status(&fresh, OffsetDateTime::now_utc()).is_none());

        let stale = entry(GrabStatus::Sent, Probe::Unknown, 30);
        let (next, reason) = next_status(&stale, OffsetDateTime::now_utc()).unwrap();
        assert_eq!(next, GrabStatus::Failed);
        assert!(reason.unwrap().contains("sumiu"));
    }

    #[test]
    fn a_reservation_is_left_to_the_delivery_path() {
        // `reserved` means nothing was handed over yet; the client is
        // supposed not to know it.
        let e = entry(GrabStatus::Reserved, Probe::Unknown, 600);
        assert!(next_status(&e, OffsetDateTime::now_utc()).is_none());
    }
}
