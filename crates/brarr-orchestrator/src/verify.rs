//! Reconciling the catalogue with the disk.
//!
//! brarr answers "do I already have this?" from `grabs` — the operator's
//! choice, taken when the scanner was built, over walking the library on
//! every sweep. That answer has one known lie: a file deleted outside
//! brarr leaves the item covered forever, because nothing ever looked.
//!
//! This looks. For every grab that reached `imported`, it checks whether
//! [`crate::db::grabs::Grab::imported_path`] is still there. A file that
//! is gone marks its grab `file_missing_at`, which does two things at
//! once:
//!
//! - the grab stops covering its item, so the scanner wants it again;
//! - the grab drops out of the barrier's partial index, so the *same*
//!   release can be acquired again — usually exactly the one an operator
//!   who deleted something by accident wants back.
//!
//! The grab's status stays `imported`, because it was: rewriting history
//! to explain the present is how audit trails become useless.
//!
//! ## What this does not do
//!
//! It does not walk the root folders looking for files brarr never
//! imported, and it does not notice a file that was replaced by a
//! different one at the same path. Both are "adopt what is already on
//! disk", which is a different feature from "verify what I put there".

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio::time::sleep;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::db::grabs;
use crate::{AppError, AppState};

/// How often the verification runs on its own.
///
/// Files do not vanish often, and each pass is one `stat` per imported
/// grab. Six hours keeps it invisible; the button on `/library` covers
/// "I just deleted something, notice now".
pub const VERIFY_INTERVAL: Duration = Duration::from_secs(6 * 3600);

/// Delay before the first pass, so it doesn't pile onto the startup burst.
const STARTUP_DELAY: Duration = Duration::from_secs(180);

/// What one pass found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerifySummary {
    /// Imported files checked.
    pub checked: usize,
    /// Files that are no longer where brarr put them.
    pub missing: usize,
}

/// Spawn the background verification.
#[must_use]
pub fn spawn(state: AppState) -> JoinHandle<()> {
    let state = Arc::new(state);
    info!(
        target: "brarr_orchestrator::verify",
        interval_secs = VERIFY_INTERVAL.as_secs(),
        "starting the library file check"
    );
    tokio::spawn(async move {
        sleep(STARTUP_DELAY).await;
        loop {
            match run_once(&state).await {
                Ok(summary) if summary.missing > 0 => info!(
                    target: "brarr_orchestrator::verify",
                    checked = summary.checked,
                    missing = summary.missing,
                    "files went missing; those items are wanted again"
                ),
                Ok(summary) => debug!(
                    target: "brarr_orchestrator::verify",
                    checked = summary.checked,
                    "library intact"
                ),
                Err(e) => {
                    warn!(target: "brarr_orchestrator::verify", error = %e, "file check failed");
                }
            }
            sleep(VERIFY_INTERVAL).await;
        }
    })
}

/// Check every imported file once.
///
/// # Errors
///
/// Returns [`AppError::Database`] when the grab list cannot be read or a
/// finding cannot be recorded.
pub async fn run_once(state: &AppState) -> Result<VerifySummary, AppError> {
    let imported = grabs::imported_present(state.pool()).await?;
    if imported.is_empty() {
        return Ok(VerifySummary::default());
    }

    let candidates: Vec<(Uuid, String)> = imported
        .into_iter()
        .filter_map(|g| g.imported_path.map(|p| (g.id, p)))
        .collect();
    let checked = candidates.len();

    // One blocking hop for the whole batch: a `stat` is fast, but on a
    // network mount it is fast *until it isn't*, and a stalled NFS call
    // on a runtime worker would take the whole process with it.
    let gone = tokio::task::spawn_blocking(move || {
        candidates
            .into_iter()
            .filter(|(_, path)| is_gone(Path::new(path)))
            .collect::<Vec<_>>()
    })
    .await
    .map_err(|e| AppError::InvalidInput(format!("verificação falhou: {e}")))?;

    let missing = gone.len();
    for (id, path) in gone {
        grabs::mark_file_missing(state.pool(), id).await?;
        warn!(
            target: "brarr_orchestrator::verify",
            grab_id = %id,
            path = %path,
            "imported file is gone; the item is wanted again"
        );
    }
    Ok(VerifySummary { checked, missing })
}

/// `true` when the path is definitely not there.
///
/// Deliberately narrow: only `NotFound` counts. A permission error, a
/// mount that is temporarily unreachable, an I/O error — none of those
/// mean the file was deleted, and treating them as such would have brarr
/// re-download a library because a disk was busy.
pub(crate) fn is_gone(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(_) => false,
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
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
    use crate::db::grabs::{GrabStatus, NewGrab, Protocol};
    use crate::db::library::{self, MediaType, NewLibraryItem};
    use crate::db::open_memory;

    async fn imported_grab(pool: &crate::db::Pool, path: &str) -> Uuid {
        let item = library::upsert(
            pool,
            &NewLibraryItem {
                media_type: Some(MediaType::Movie),
                tmdb_id: 603,
                title: "The Matrix".to_owned(),
                ..NewLibraryItem::default()
            },
        )
        .await
        .unwrap();
        let provider = crate::db::providers::insert(
            pool,
            crate::db::providers::NewProvider {
                name: "capybara",
                base_url: &url::Url::parse("https://capybarabr.com/").unwrap(),
                api_token: "tok",
                kind: "unit3d",
                plugin_path: None,
            },
        )
        .await
        .unwrap();
        let grab = grabs::reserve(
            pool,
            &NewGrab {
                item_id: item.id,
                episode_id: None,
                season_number: None,
                decision_id: None,
                provider_id: provider.id,
                provider_name: "capybara",
                release_id_remote: "abc",
                release_name: "Matrix.1999.1080p",
                download_url: None,
                protocol: Protocol::Torrent,
            },
        )
        .await
        .unwrap()
        .unwrap();
        grabs::mark_imported(pool, grab.id, path).await.unwrap();
        grab.id
    }

    fn state(pool: crate::db::Pool) -> AppState {
        AppState::new(pool, brarr_decision_service::Engine::baseline())
    }

    #[tokio::test]
    async fn a_file_still_there_is_left_alone() {
        let pool = open_memory().await.unwrap();
        let dir = std::env::temp_dir().join(format!("brarr-verify-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("Matrix (1999).mkv");
        std::fs::write(&file, b"x").unwrap();

        let id = imported_grab(&pool, file.to_str().unwrap()).await;
        let summary = run_once(&state(pool.clone())).await.unwrap();
        assert_eq!(
            summary,
            VerifySummary {
                checked: 1,
                missing: 0
            }
        );
        assert!(
            grabs::get_by_id(&pool, id)
                .await
                .unwrap()
                .file_missing_at
                .is_none()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_deleted_file_makes_its_item_wanted_again() {
        let pool = open_memory().await.unwrap();
        let dir = std::env::temp_dir().join(format!("brarr-verify-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("Matrix (1999).mkv");
        std::fs::write(&file, b"x").unwrap();
        let id = imported_grab(&pool, file.to_str().unwrap()).await;

        // The operator deletes it outside brarr — the case the whole
        // module exists for.
        std::fs::remove_file(&file).unwrap();

        let summary = run_once(&state(pool.clone())).await.unwrap();
        assert_eq!(
            summary,
            VerifySummary {
                checked: 1,
                missing: 1
            }
        );

        let after = grabs::get_by_id(&pool, id).await.unwrap();
        assert!(after.file_missing_at.is_some());
        assert_eq!(
            after.status,
            GrabStatus::Imported,
            "it was imported; rewriting that would erase what happened"
        );
        assert!(
            grabs::blocking_for(&pool, after.item_id, grabs::GrabTarget::item())
                .await
                .unwrap()
                .is_empty(),
            "the item is wanted again"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_second_pass_does_not_re_report_what_it_already_found() {
        let pool = open_memory().await.unwrap();
        let id = imported_grab(&pool, "/caminho/que/nunca/existiu.mkv").await;
        let state = state(pool.clone());

        assert_eq!(run_once(&state).await.unwrap().missing, 1);
        assert_eq!(
            run_once(&state).await.unwrap(),
            VerifySummary::default(),
            "a known-missing file leaves the working set"
        );
        assert!(
            grabs::get_by_id(&pool, id)
                .await
                .unwrap()
                .file_missing_at
                .is_some()
        );
    }

    #[test]
    fn only_a_missing_file_counts_as_gone() {
        let dir = std::env::temp_dir().join(format!("brarr-verify-gone-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("present.mkv");
        std::fs::write(&file, b"x").unwrap();

        assert!(!is_gone(&file));
        assert!(is_gone(&dir.join("nunca-existiu.mkv")));
        // A directory at the path is not a missing file — whatever else
        // it is, it is not grounds to re-download a library.
        assert!(!is_gone(&dir));
        std::fs::remove_dir_all(&dir).ok();
    }
}
