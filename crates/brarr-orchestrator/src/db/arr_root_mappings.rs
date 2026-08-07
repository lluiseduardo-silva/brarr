//! `arr_root_mappings` — what a Sonarr/Radarr calls a root folder, and
//! which of brarr's roots is the same place on disk.
//!
//! See `migrations/20260807130000_arr_import.sql` for the schema
//! rationale. The rule itself is [`crate::remote_path`]'s, shared with
//! `path_mappings`: same question, same sharp edges, one implementation.
//!
//! Two conventions carried over from `path_mappings`, for the same
//! reasons:
//!
//! - **The \*arr side never touches the filesystem.** It names a
//!   directory as another container sees it and *should* not exist here
//!   — that is why the row exists at all. Only the brarr side is
//!   validated, and `root_folders` already did that at registration.
//! - **There is no update path.** A mapping is added or removed, which
//!   keeps the stored `arr_path` canonical by construction so the
//!   matching never compares against a half-normalised string.
//!
//! Unlike `path_mappings`, the row is read back with its root folder
//! joined in: a mapping whose local side is unknown cannot translate
//! anything, and the foreign key guarantees the join finds a row.

use std::path::PathBuf;

use sqlx::{Row, sqlite::SqliteRow};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::db::{arr_instances, root_folders};
use crate::remote_path::PrefixRule;
use crate::{AppError, db::Pool};

/// One \*arr root → brarr root rewrite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArrRootMapping {
    /// Stable UUID v4, used in the delete route.
    pub id: Uuid,
    /// Which \*arr reports paths in this namespace.
    pub arr_instance_id: Uuid,
    /// Prefix as the \*arr writes it — `/data/Series` on the operator's
    /// stack. A `String`, not a `PathBuf`: it is not a path here.
    pub arr_path: String,
    /// The brarr root folder it corresponds to.
    pub root_folder_id: Uuid,
    /// That folder's path, joined in — a mapping is useless without it.
    pub root_path: PathBuf,
    /// Row creation timestamp.
    pub created_at: OffsetDateTime,
}

impl ArrRootMapping {
    /// The rewrite rule alone, for [`crate::remote_path::translate`].
    #[must_use]
    pub fn rule(&self) -> PrefixRule {
        PrefixRule {
            id: self.id,
            remote_prefix: self.arr_path.clone(),
            local_prefix: self.root_path.clone(),
        }
    }
}

/// Every mapping of an instance as rewrite rules, ready to translate.
#[must_use]
pub fn rules(mappings: &[ArrRootMapping]) -> Vec<PrefixRule> {
    mappings.iter().map(ArrRootMapping::rule).collect()
}

const COLUMNS: &str = "m.id, m.arr_instance_id, m.arr_path, m.root_folder_id, m.created_at, \
     r.path AS root_path";

const FROM: &str = "FROM arr_root_mappings m JOIN root_folders r ON r.id = m.root_folder_id";

fn row_to_mapping(row: &SqliteRow) -> Result<ArrRootMapping, AppError> {
    let id: String = row.try_get("id")?;
    let instance: String = row.try_get("arr_instance_id")?;
    let root_folder: String = row.try_get("root_folder_id")?;
    let root_path: String = row.try_get("root_path")?;
    let created_at: i64 = row.try_get("created_at")?;
    Ok(ArrRootMapping {
        id: Uuid::parse_str(&id)
            .map_err(|e| AppError::InvalidInput(format!("invalid arr_root_mapping id: {e}")))?,
        arr_instance_id: Uuid::parse_str(&instance)
            .map_err(|e| AppError::InvalidInput(format!("invalid arr_instance id: {e}")))?,
        arr_path: row.try_get("arr_path")?,
        root_folder_id: Uuid::parse_str(&root_folder)
            .map_err(|e| AppError::InvalidInput(format!("invalid root_folder id: {e}")))?,
        root_path: PathBuf::from(root_path),
        created_at: OffsetDateTime::from_unix_timestamp(created_at)
            .map_err(|e| AppError::InvalidInput(format!("invalid created_at: {e}")))?,
    })
}

/// Register a mapping.
///
/// Both foreign keys are read first, so a missing instance or a deleted
/// root folder answers 404 with a sentence instead of the 500 a raw
/// foreign-key violation would give.
///
/// # Errors
///
/// - [`AppError::NotFound`] when the instance or the root folder is gone.
/// - [`AppError::InvalidInput`] when `arr_path` cannot serve as a prefix,
///   or when this instance already maps it.
/// - [`AppError::Database`] on any other SQL failure.
pub async fn insert(
    pool: &Pool,
    arr_instance_id: Uuid,
    arr_path: &str,
    root_folder_id: Uuid,
) -> Result<ArrRootMapping, AppError> {
    arr_instances::get_by_id(pool, arr_instance_id).await?;
    let root = root_folders::get_by_id(pool, root_folder_id).await?;

    // The same canonical form `path_mappings` stores, and refusing the
    // bare POSIX root for the same reason: it would cover every path the
    // *arr could report and turn one mapping into a global rewrite.
    let prefix = crate::remote_path::canonical_prefix(arr_path).ok_or_else(|| {
        AppError::InvalidInput(
            "o caminho no *arr precisa ser absoluto no formato dele — /data/Series, \
             C:\\Midia ou \\\\NAS\\midia — e não pode ser só \"/\", que casaria com tudo"
                .into(),
        )
    })?;

    let id = Uuid::new_v4();
    let now = OffsetDateTime::now_utc();
    let result = sqlx::query(
        "INSERT INTO arr_root_mappings (id, arr_instance_id, arr_path, root_folder_id, created_at) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(id.to_string())
    .bind(arr_instance_id.to_string())
    .bind(&prefix)
    .bind(root_folder_id.to_string())
    .bind(now.unix_timestamp())
    .execute(pool)
    .await;

    if let Err(sqlx::Error::Database(db)) = &result {
        if db.is_unique_violation() {
            return Err(AppError::InvalidInput(format!(
                "esta instância já tem um mapeamento para {prefix}"
            )));
        }
    }
    result?;

    Ok(ArrRootMapping {
        id,
        arr_instance_id,
        arr_path: prefix,
        root_folder_id,
        root_path: root.path,
        created_at: now,
    })
}

/// Every mapping of one instance — what the import reads, once per run.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn for_instance(
    pool: &Pool,
    arr_instance_id: Uuid,
) -> Result<Vec<ArrRootMapping>, AppError> {
    let rows = sqlx::query(&format!(
        "SELECT {COLUMNS} {FROM} WHERE m.arr_instance_id = ? \
         ORDER BY m.created_at ASC, m.id ASC"
    ))
    .bind(arr_instance_id.to_string())
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_mapping).collect()
}

/// Every mapping, for the admin screen.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn list_all(pool: &Pool) -> Result<Vec<ArrRootMapping>, AppError> {
    let rows = sqlx::query(&format!(
        "SELECT {COLUMNS} {FROM} ORDER BY m.arr_instance_id ASC, m.arr_path ASC"
    ))
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_mapping).collect()
}

/// Remove a mapping. `true` when a row went away.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn delete_by_id(pool: &Pool, id: Uuid) -> Result<bool, AppError> {
    let res = sqlx::query("DELETE FROM arr_root_mappings WHERE id = ?")
        .bind(id.to_string())
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
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
    use brarr_arr::ArrKind;
    use url::Url;

    /// A directory that exists for the duration of one test — the same
    /// shape `root_folders`' own tests use, so no new dependency.
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("brarr-arrmap-{name}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn instance(pool: &Pool, name: &str) -> Uuid {
        let url = Url::parse("https://sonarr.example/").unwrap();
        arr_instances::insert(
            pool,
            arr_instances::NewArrInstance {
                name,
                kind: ArrKind::Sonarr,
                base_url: &url,
                api_key: "k",
                push_threshold: None,
                profile_id: None,
                enabled: Some(false),
            },
        )
        .await
        .unwrap()
        .id
    }

    async fn root(pool: &Pool, dir: &std::path::Path) -> Uuid {
        root_folders::insert(pool, &dir.to_string_lossy(), None)
            .await
            .unwrap()
            .id
    }

    #[tokio::test]
    async fn a_mapping_translates_the_production_case() {
        let pool = open_memory().await.unwrap();
        let dir = temp_dir("translate");
        let inst = instance(&pool, "sonarr-series").await;
        let root_id = root(&pool, &dir).await;

        insert(&pool, inst, "/data/Series", root_id).await.unwrap();
        let mappings = for_instance(&pool, inst).await.unwrap();

        // Exactly the shape measured on the operator's stack: Sonarr says
        // /data/Series, brarr mounts the same share somewhere else.
        let out = crate::remote_path::translate(
            &rules(&mappings),
            "/data/Series/9-1-1/Season 1/9-1-1 - S01E01.mkv",
        );
        assert_eq!(
            out.local,
            dir.join("9-1-1")
                .join("Season 1")
                .join("9-1-1 - S01E01.mkv")
        );
        assert!(out.applied.is_some(), "the rule must have fired");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn the_root_folder_path_comes_back_with_the_row() {
        let pool = open_memory().await.unwrap();
        let dir = temp_dir("joined");
        let inst = instance(&pool, "radarr").await;
        let root_id = root(&pool, &dir).await;
        insert(&pool, inst, "/data/Filmes", root_id).await.unwrap();

        let mappings = for_instance(&pool, inst).await.unwrap();
        assert_eq!(mappings.len(), 1);
        assert_eq!(mappings[0].root_path, dir);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn the_bare_root_is_refused() {
        let pool = open_memory().await.unwrap();
        let dir = temp_dir("bare");
        let inst = instance(&pool, "sonarr").await;
        let root_id = root(&pool, &dir).await;
        // `/` would cover every path the *arr could report.
        let err = insert(&pool, inst, "/", root_id).await.unwrap_err();
        assert!(matches!(err, AppError::InvalidInput(_)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn the_same_prefix_twice_is_form_input_not_a_500() {
        let pool = open_memory().await.unwrap();
        let dir = temp_dir("dupe");
        let inst = instance(&pool, "sonarr").await;
        let root_id = root(&pool, &dir).await;
        insert(&pool, inst, "/data/Series", root_id).await.unwrap();
        // Trailing separator and all: the stored form is canonical, so
        // this is the same row.
        let err = insert(&pool, inst, "/data/Series/", root_id)
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::InvalidInput(_)), "got {err:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_missing_instance_is_a_404() {
        let pool = open_memory().await.unwrap();
        let dir = temp_dir("noinst");
        let root_id = root(&pool, &dir).await;
        let err = insert(&pool, Uuid::new_v4(), "/data/Series", root_id)
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_missing_root_folder_is_a_404() {
        let pool = open_memory().await.unwrap();
        let inst = instance(&pool, "sonarr").await;
        let err = insert(&pool, inst, "/data/Series", Uuid::new_v4())
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)));
    }

    /// `ON DELETE CASCADE` on both sides: a mapping without its instance
    /// is dead configuration, not history worth keeping.
    #[tokio::test]
    async fn deleting_the_instance_takes_its_mappings() {
        let pool = open_memory().await.unwrap();
        let dir = temp_dir("cascade-inst");
        let inst = instance(&pool, "sonarr").await;
        let root_id = root(&pool, &dir).await;
        insert(&pool, inst, "/data/Series", root_id).await.unwrap();

        arr_instances::delete_by_id(&pool, inst).await.unwrap();
        assert!(list_all(&pool).await.unwrap().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn deleting_the_root_folder_takes_its_mappings() {
        let pool = open_memory().await.unwrap();
        let dir = temp_dir("cascade-root");
        let inst = instance(&pool, "sonarr").await;
        let root_id = root(&pool, &dir).await;
        insert(&pool, inst, "/data/Series", root_id).await.unwrap();

        root_folders::delete_by_id(&pool, root_id).await.unwrap();
        assert!(list_all(&pool).await.unwrap().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn delete_reports_whether_a_row_went_away() {
        let pool = open_memory().await.unwrap();
        let dir = temp_dir("delete");
        let inst = instance(&pool, "sonarr").await;
        let root_id = root(&pool, &dir).await;
        let row = insert(&pool, inst, "/data/Series", root_id).await.unwrap();
        assert!(delete_by_id(&pool, row.id).await.unwrap());
        assert!(!delete_by_id(&pool, row.id).await.unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }
}
