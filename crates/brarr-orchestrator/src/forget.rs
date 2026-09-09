//! Undo one acquisition without erasing that it happened.
//!
//! # Why this exists
//!
//! A bad import used to have no way out of the UI at all. `adopt::undo`
//! and `grabs::delete_adopted` both refuse anything that is not
//! `protocol = 'local'`, and that refusal is **right**: an adoption
//! created no acquisition history, while a tracker grab *is* the history.
//! `requeue-import` only moves `failed → completed`, so an `imported`
//! row it cannot reach.
//!
//! The gap was found the hard way. A season pack of Cowboy Bebop
//! imported one file and, through `scope = 'season'`, claimed all 26
//! episodes. The disk-import screen then marked all 26 real files "já na
//! biblioteca" and refused to offer them, because a live grab covered
//! their episodes. Every route was closed and the only remaining repair
//! was editing sqlite by hand.
//!
//! # What it does, and what it deliberately does not
//!
//! It **does not delete the row**. The record of what was downloaded
//! outlives the decision to undo it, which is the rule
//! `delete_adopted_refuses_a_tracker_grab` has always encoded. It marks
//! `file_missing_at`, and that is not a euphemism: by the time it is
//! written, the file really is gone, because this removed it. The mark
//! does exactly the three things needed — keeps the history, drops the
//! row out of coverage, and frees the partial unique index so the same
//! release can be acquired again — with no new state at all.
//!
//! # The rule for removing a file
//!
//! A library file is removed **only when the data survives without it**:
//! the file has more than one link, so some other name — the download
//! still seeding, in practice — refers to the same bytes. That is the
//! property that makes the removal free, and it is checkable on the spot
//! with one `stat`.
//!
//! `adopt::undo`'s inode comparison against the source is the wrong tool
//! here, twice over. `release_id_remote` is the source path only for a
//! local adoption; on a tracker grab it is a remote id like `10731`, so
//! there is nothing to compare against without going back to the download
//! client. And `import::place` may copy or move, and a copy has a
//! different inode by construction — so the comparison would refuse
//! exactly the files it was meant to clear.
//!
//! A file with one link is refused and named. It may be the operator's
//! only copy, and brarr cannot tell from here whether the download that
//! produced it still exists. Refusing costs one manual `rm`; guessing
//! costs the file.

use std::path::PathBuf;

use tracing::{info, warn};
use uuid::Uuid;

use crate::db::grabs::{self, Grab, GrabStatus};
use crate::{AppError, AppState};

/// What forgetting one acquisition did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Forgotten {
    /// Rows that stopped covering anything. The history stays.
    pub rows: usize,
    /// Files brarr had written, removed.
    pub removed: Vec<PathBuf>,
    /// Files it refused to remove, and why.
    ///
    /// Reported rather than swallowed: a refusal means brarr could not
    /// prove the bytes survive without that name, and the operator is the
    /// only one who can say whether to delete it anyway.
    pub refused: Vec<(PathBuf, String)>,
}

impl Forgotten {
    /// One sentence for the badge.
    #[must_use]
    pub fn summary(&self) -> String {
        use std::fmt::Write as _;

        let mut said = format!(
            "{} aquisição(ões) esquecida(s), {} arquivo(s) removido(s)",
            self.rows,
            self.removed.len()
        );
        if !self.refused.is_empty() {
            // Writing into a `String` cannot fail, and swallowing the
            // `Result` here is not hiding anything.
            let _ = write!(
                said,
                ", {} não removido(s) — confira o histórico",
                self.refused.len()
            );
        }
        said
    }
}

/// Forget one acquisition, and the rows a fan-out wrote for it.
///
/// # Errors
///
/// [`AppError::NotFound`] when the grab is gone,
/// [`AppError::InvalidInput`] when it is not an imported grab, and
/// [`AppError::Database`] on SQL failure.
pub async fn forget(state: &AppState, grab_id: Uuid) -> Result<Forgotten, AppError> {
    let grab = grabs::get_by_id(state.pool(), grab_id).await?;
    // A download still in flight was reserved against a target, and
    // cancelling one is a different feature with a different question to
    // answer (does the client keep the data?). Refusing here keeps this
    // action about files that exist.
    if grab.status != GrabStatus::Imported {
        return Err(AppError::InvalidInput(format!(
            "só dá para esquecer uma aquisição já importada — esta está {}",
            grab.status.label()
        )));
    }

    // The pack first, then the rows it wrote: forgetting a pack has to
    // take its episodes with it, or the children would go on covering
    // episodes whose files this just removed.
    let mut rows = vec![grab.clone()];
    rows.extend(grabs::children_of(state.pool(), grab.id).await?);

    let mut out = Forgotten::default();
    for row in &rows {
        if let Some(path) = row.imported_path.clone() {
            match remove(row, &path).await? {
                Ok(Some(removed)) => out.removed.push(removed),
                Ok(None) => {}
                Err(why) => out.refused.push((PathBuf::from(&path), why)),
            }
        }
        // Marked even when the removal was refused. The file is not
        // brarr's any more either way, and leaving the row covering the
        // episode is what kept the operator stuck.
        grabs::mark_file_missing(state.pool(), row.id).await?;
        out.rows += 1;
    }

    info!(
        target: "brarr_orchestrator::forget",
        grab_id = %grab.id,
        release = %grab.release_name,
        rows = out.rows,
        removed = out.removed.len(),
        refused = out.refused.len(),
        "acquisition forgotten"
    );
    Ok(out)
}

/// Remove the file this row recorded, if removing it loses nothing.
///
/// `Ok(None)` when there was nothing to remove — an in-place adoption,
/// which by construction wrote nothing, or a path that is already gone.
async fn remove(row: &Grab, path: &str) -> Result<Result<Option<PathBuf>, String>, AppError> {
    // An in-place adoption's `imported_path` *is* the operator's own
    // file. `adopt::undo` makes the same distinction and for the same
    // reason: brarr wrote nothing, so forgetting writes nothing back.
    if grabs::is_in_place(row) {
        return Ok(Ok(None));
    }
    let target = PathBuf::from(path);
    let done = tokio::task::spawn_blocking(move || remove_if_spare(&target))
        .await
        .map_err(|e| AppError::InvalidInput(format!("remoção falhou: {e}")))?;
    Ok(match done {
        Ok(removed) => Ok(removed),
        Err(why) => {
            warn!(
                target: "brarr_orchestrator::forget",
                grab_id = %row.id,
                path = %path,
                reason = %why,
                "refused to remove a library file"
            );
            Err(why)
        }
    })
}

/// Remove `path` when another name still refers to the same bytes.
#[cfg(unix)]
fn remove_if_spare(path: &std::path::Path) -> Result<Option<PathBuf>, String> {
    use std::os::unix::fs::MetadataExt as _;

    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("não consegui ler {}: {e}", path.display())),
    };
    if meta.nlink() < 2 {
        return Err(format!(
            "{} é a única cópia desses bytes — o brarr não remove um arquivo              que não pode provar que sobrevive. Apague à mão se for o caso.",
            path.display()
        ));
    }
    std::fs::remove_file(path)
        .map(|()| Some(path.to_path_buf()))
        .map_err(|e| format!("não consegui remover {}: {e}", path.display()))
}

/// Windows has hard links but no portable link count through `std`, and
/// this repository deploys on Linux. Refusing and naming the path is the
/// same answer `adopt::same_file` gives here, for the same reason.
#[cfg(not(unix))]
fn remove_if_spare(path: &std::path::Path) -> Result<Option<PathBuf>, String> {
    if !path.exists() {
        return Ok(None);
    }
    Err(format!(
        "nesta plataforma o brarr não consegue confirmar que {} sobrevive à remoção;          apague à mão",
        path.display()
    ))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert on happy paths"
)]
mod tests {
    use super::*;

    /// A directory that lives for one test.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("brarr-forget-{name}-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The production shape: the library file is a hardlink of the file
    /// still in the download folder, so removing it frees nothing and
    /// loses nothing. Verified on this operator's disk — all three broken
    /// packs had `nlink = 2`.
    #[cfg(unix)]
    #[test]
    fn a_library_file_that_is_still_seeding_is_removed() {
        let dir = TempDir::new("linked");
        let download = dir.path().join("Cowboy Bebop S01E23.mkv");
        std::fs::write(&download, b"bytes").unwrap();
        let library = dir.path().join("Cowboy Bebop.mkv");
        std::fs::hard_link(&download, &library).unwrap();

        assert_eq!(remove_if_spare(&library).unwrap(), Some(library.clone()));
        assert!(!library.exists(), "the library copy is gone");
        assert!(download.is_file(), "the download is untouched and seeding");
    }

    /// One link means brarr cannot prove the bytes exist anywhere else.
    /// Refusing costs a manual `rm`; guessing costs the file.
    #[cfg(unix)]
    #[test]
    fn the_only_copy_of_a_file_is_refused_and_named() {
        let dir = TempDir::new("only");
        let lonely = dir.path().join("Tremembé.mkv");
        std::fs::write(&lonely, b"bytes").unwrap();

        let refused = remove_if_spare(&lonely).unwrap_err();
        assert!(refused.contains("única cópia"), "{refused}");
        assert!(
            refused.contains("Tremembé.mkv"),
            "the path is named: {refused}"
        );
        assert!(lonely.is_file(), "nothing was removed");
    }

    /// A path that is already gone is not an error. The operator may have
    /// deleted it by hand before pressing the button, which is exactly
    /// what they had to do before this existed.
    #[test]
    fn a_path_that_is_already_gone_is_not_a_failure() {
        let dir = TempDir::new("absent");
        assert_eq!(remove_if_spare(&dir.path().join("nada.mkv")).unwrap(), None);
    }

    #[test]
    fn the_summary_says_what_was_refused() {
        let quiet = Forgotten {
            rows: 3,
            removed: vec![PathBuf::from("/a.mkv")],
            refused: Vec::new(),
        };
        assert!(!quiet.summary().contains("não removido"));

        let loud = Forgotten {
            rows: 1,
            removed: Vec::new(),
            refused: vec![(PathBuf::from("/b.mkv"), "só cópia".to_owned())],
        };
        assert!(
            loud.summary().contains("não removido"),
            "{}",
            loud.summary()
        );
    }
}
