//! `root_folders` — where the library lives on disk.
//!
//! See `migrations/20260804150000_root_folders.sql` for the schema
//! rationale. This module owns two things the import phase depends on:
//! the list of destinations, and [`resolve`], the rule that picks one
//! for a given item.
//!
//! **The path is validated when it is registered, not when it is used.**
//! A typo discovered at import time is a download that already finished,
//! occupying disk, with nowhere to go — and the operator finds out from
//! a failed grab hours later. Checking at insert costs three syscalls
//! and turns that into a form error.

use std::path::{Path, PathBuf};

use sqlx::{Row, sqlite::SqliteRow};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::db::library::MediaType;
use crate::{AppError, db::Pool};

/// One configured destination.
#[derive(Debug, Clone)]
pub struct RootFolder {
    /// Stable UUID v4.
    pub id: Uuid,
    /// Absolute path on the machine running brarr. In Docker that is the
    /// path *inside* the container, which is also what the download
    /// client must see for a hardlink to be possible.
    pub path: PathBuf,
    /// Which kind of media lands here. `None` serves both.
    pub media_type: Option<MediaType>,
    /// Row creation timestamp.
    pub created_at: OffsetDateTime,
}

/// How much room is left where a root folder points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskUsage {
    /// Bytes available to this process.
    pub available: u64,
    /// Size of the filesystem.
    pub total: u64,
}

impl DiskUsage {
    /// Percentage of the filesystem in use, `0..=100`. `0` when the
    /// total is unknown, so a missing reading renders as an empty bar
    /// rather than a full one.
    #[must_use]
    pub fn used_percent(self) -> u8 {
        if self.total == 0 {
            return 0;
        }
        let used = self.total.saturating_sub(self.available);
        // Integer maths: `used * 100` cannot overflow a u128, and the
        // result is bounded by construction.
        let pct = u128::from(used) * 100 / u128::from(self.total);
        u8::try_from(pct).unwrap_or(100).min(100)
    }
}

impl RootFolder {
    /// Free space where this folder points.
    ///
    /// Returns `None` when the filesystem cannot be queried — the folder
    /// was unmounted since it was registered, say. The UI shows nothing
    /// rather than a zero that reads like a full disk.
    #[must_use]
    pub fn usage(&self) -> Option<DiskUsage> {
        let available = fs4::available_space(&self.path).ok()?;
        let total = fs4::total_space(&self.path).ok()?;
        Some(DiskUsage { available, total })
    }
}

/// Normalise an operator-typed path: trimmed, and without a trailing
/// separator so `/data/media` and `/data/media/` are the same row.
fn normalise(raw: &str) -> PathBuf {
    let trimmed = raw.trim();
    let stripped = trimmed
        .strip_suffix(['/', '\\'])
        .filter(|s| !s.is_empty())
        .unwrap_or(trimmed);
    PathBuf::from(stripped)
}

/// Confirm the path can actually receive an import.
///
/// # Errors
///
/// [`AppError::InvalidInput`] when the path is blank, does not exist, is
/// not a directory, or cannot be written to.
fn validate(path: &Path) -> Result<(), AppError> {
    if path.as_os_str().is_empty() {
        return Err(AppError::InvalidInput(
            "o caminho da pasta raiz não pode ser vazio".into(),
        ));
    }
    let meta = std::fs::metadata(path).map_err(|e| {
        AppError::InvalidInput(format!("não consegui acessar {}: {e}", path.display()))
    })?;
    if !meta.is_dir() {
        return Err(AppError::InvalidInput(format!(
            "{} existe mas não é um diretório",
            path.display()
        )));
    }
    if meta.permissions().readonly() {
        return Err(AppError::InvalidInput(format!(
            "{} está somente-leitura para o usuário que roda o brarr",
            path.display()
        )));
    }
    Ok(())
}

const COLUMNS: &str = "id, path, media_type, created_at";

/// Register a root folder.
///
/// # Errors
///
/// - [`AppError::InvalidInput`] when the path fails [`validate`].
/// - [`AppError::Database`] on `UNIQUE(path)` violation or SQL failure.
pub async fn insert(
    pool: &Pool,
    raw_path: &str,
    media_type: Option<MediaType>,
) -> Result<RootFolder, AppError> {
    let path = normalise(raw_path);
    validate(&path)?;

    let id = Uuid::new_v4();
    let now = OffsetDateTime::now_utc();
    sqlx::query("INSERT INTO root_folders (id, path, media_type, created_at) VALUES (?, ?, ?, ?)")
        .bind(id.to_string())
        .bind(path.to_string_lossy().as_ref())
        .bind(media_type.map(MediaType::label))
        .bind(now.unix_timestamp())
        .execute(pool)
        .await?;
    Ok(RootFolder {
        id,
        path,
        media_type,
        created_at: now,
    })
}

/// Every root folder, typed ones first then by path.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn list_all(pool: &Pool) -> Result<Vec<RootFolder>, AppError> {
    let rows = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM root_folders ORDER BY media_type IS NULL, media_type, path"
    ))
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_folder).collect()
}

/// Fetch one by id.
///
/// # Errors
///
/// Returns [`AppError::NotFound`] when absent.
pub async fn get_by_id(pool: &Pool, id: Uuid) -> Result<RootFolder, AppError> {
    let row = sqlx::query(&format!("SELECT {COLUMNS} FROM root_folders WHERE id = ?"))
        .bind(id.to_string())
        .fetch_optional(pool)
        .await?;
    match row {
        Some(r) => row_to_folder(&r),
        None => Err(AppError::NotFound(format!("root_folder {id}"))),
    }
}

/// Delete by id. Returns `true` when a row was removed.
///
/// Nothing on disk is touched: this forgets a destination, it does not
/// unmake a library.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn delete_by_id(pool: &Pool, id: Uuid) -> Result<bool, AppError> {
    let res = sqlx::query("DELETE FROM root_folders WHERE id = ?")
        .bind(id.to_string())
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// Pick the folder an item of `media_type` should land in.
///
/// An exact match wins over an untyped folder, and among equals the
/// first by path — deterministic, so the same item always resolves to
/// the same place. `None` means nothing is configured, which the import
/// path must report rather than guess a destination for.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn resolve(pool: &Pool, media_type: MediaType) -> Result<Option<RootFolder>, AppError> {
    let folders = list_all(pool).await?;
    Ok(pick(&folders, media_type).cloned())
}

/// Pure half of [`resolve`], so the rule is testable without a pool.
fn pick(folders: &[RootFolder], media_type: MediaType) -> Option<&RootFolder> {
    folders
        .iter()
        .find(|f| f.media_type == Some(media_type))
        .or_else(|| folders.iter().find(|f| f.media_type.is_none()))
}

fn row_to_folder(row: &SqliteRow) -> Result<RootFolder, AppError> {
    let id_raw: String = row.try_get("id")?;
    let id = Uuid::parse_str(&id_raw)
        .map_err(|e| AppError::InvalidInput(format!("invalid uuid in root_folders.id: {e}")))?;
    let path: String = row.try_get("path")?;
    let media_raw: Option<String> = row.try_get("media_type")?;
    let media_type = media_raw
        .as_deref()
        .map(MediaType::from_label)
        .transpose()?;
    let created: i64 = row.try_get("created_at")?;
    Ok(RootFolder {
        id,
        path: PathBuf::from(path),
        media_type,
        created_at: OffsetDateTime::from_unix_timestamp(created)
            .map_err(|e| AppError::InvalidInput(format!("invalid root_folders.created_at: {e}")))?,
    })
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert on happy paths"
)]
mod tests {
    use super::*;
    use crate::db::open_memory;

    /// A directory that exists for the duration of one test.
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("brarr-root-{name}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn folder(media_type: Option<MediaType>, path: &str) -> RootFolder {
        RootFolder {
            id: Uuid::new_v4(),
            path: PathBuf::from(path),
            media_type,
            created_at: OffsetDateTime::now_utc(),
        }
    }

    #[tokio::test]
    async fn insert_and_list_roundtrips() {
        let pool = open_memory().await.unwrap();
        let dir = temp_dir("roundtrip");
        let row = insert(&pool, dir.to_str().unwrap(), Some(MediaType::Movie))
            .await
            .unwrap();
        assert_eq!(row.media_type, Some(MediaType::Movie));
        assert_eq!(row.path, dir);
        assert_eq!(list_all(&pool).await.unwrap().len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_trailing_separator_is_the_same_folder() {
        let pool = open_memory().await.unwrap();
        let dir = temp_dir("trailing");
        let with_slash = format!("{}/", dir.to_str().unwrap());
        insert(&pool, dir.to_str().unwrap(), None).await.unwrap();
        let err = insert(&pool, &with_slash, None).await.unwrap_err();
        assert!(
            matches!(err, AppError::Database(_)),
            "the UNIQUE index has to see one path, not two spellings"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_path_that_does_not_exist_is_refused_at_registration() {
        let pool = open_memory().await.unwrap();
        // The whole point of validating here: this same typo discovered
        // at import time is a finished download with nowhere to go.
        let err = insert(&pool, "/nao/existe/em/lugar/nenhum", None)
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn a_file_is_not_a_root_folder() {
        let pool = open_memory().await.unwrap();
        let dir = temp_dir("file");
        let file = dir.join("nao-sou-pasta.txt");
        std::fs::write(&file, b"x").unwrap();
        let err = insert(&pool, file.to_str().unwrap(), None)
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::InvalidInput(_)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_blank_path_is_refused() {
        let pool = open_memory().await.unwrap();
        assert!(matches!(
            insert(&pool, "   ", None).await.unwrap_err(),
            AppError::InvalidInput(_)
        ));
    }

    #[tokio::test]
    async fn delete_returns_true_only_when_a_row_existed() {
        let pool = open_memory().await.unwrap();
        let dir = temp_dir("delete");
        let row = insert(&pool, dir.to_str().unwrap(), None).await.unwrap();
        assert!(delete_by_id(&pool, row.id).await.unwrap());
        assert!(!delete_by_id(&pool, row.id).await.unwrap());
        assert!(dir.exists(), "forgetting a destination is not unmaking it");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_exact_kind_wins_over_a_folder_that_serves_both() {
        let folders = vec![
            folder(None, "/data/media"),
            folder(Some(MediaType::Movie), "/data/filmes"),
        ];
        assert_eq!(
            pick(&folders, MediaType::Movie).map(|f| f.path.as_path()),
            Some(Path::new("/data/filmes"))
        );
        // …and the untyped one still catches what has no home of its own.
        assert_eq!(
            pick(&folders, MediaType::Tv).map(|f| f.path.as_path()),
            Some(Path::new("/data/media"))
        );
    }

    #[test]
    fn nothing_configured_resolves_to_nothing() {
        // The import path has to say "no root folder" rather than invent
        // one — writing user files to a guessed directory is the worst
        // possible failure here.
        assert!(pick(&[], MediaType::Movie).is_none());
        let only_tv = vec![folder(Some(MediaType::Tv), "/data/series")];
        assert!(pick(&only_tv, MediaType::Movie).is_none());
    }

    #[test]
    fn usage_percentage_is_bounded_and_survives_a_zero_total() {
        assert_eq!(
            DiskUsage {
                available: 25,
                total: 100
            }
            .used_percent(),
            75
        );
        assert_eq!(
            DiskUsage {
                available: 0,
                total: 0
            }
            .used_percent(),
            0,
            "an unknown filesystem must not render as a full one"
        );
        assert_eq!(
            DiskUsage {
                available: 200,
                total: 100
            }
            .used_percent(),
            0,
            "more free than total is nonsense, not 100% used"
        );
    }
}
