//! `path_mappings` — what the download client calls a directory, and
//! what brarr calls the same place on disk.
//!
//! See `migrations/20260806120000_path_mappings.sql` for the schema
//! rationale. The rule itself lives in [`crate::remote_path`], which is
//! pure; this module only stores and serves the rows.
//!
//! Two conventions worth knowing before editing:
//!
//! - **The remote side never touches the filesystem.** It names a
//!   directory on another machine, possibly under another operating
//!   system's rules, and it *should* not exist here — that is the whole
//!   reason the row exists. Only [`NewPathMapping::local_prefix`] is
//!   validated.
//! - **There is no update path.** A mapping is added or removed, like a
//!   root folder. That keeps the stored `remote_prefix` canonical by
//!   construction, so the matching never has to compare against a
//!   half-normalised string.

use std::path::{Path, PathBuf};

use sqlx::{Row, sqlite::SqliteRow};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::db::download_clients;
use crate::{AppError, db::Pool};

/// One remote → local prefix rewrite, for one client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathMapping {
    /// Stable UUID v4, used in the delete route.
    pub id: Uuid,
    /// Which client reports paths in this namespace. The key is the
    /// client row, not a hostname — see the migration.
    pub client_id: Uuid,
    /// Prefix as the *client* writes it. A `String`, not a `PathBuf`:
    /// it is not a path on this machine, and `PathBuf` would invite
    /// exactly the native operations that must never touch it.
    pub remote_prefix: String,
    /// Prefix in brarr's namespace. A `PathBuf`, because it is one.
    pub local_prefix: PathBuf,
    /// Row creation timestamp.
    pub created_at: OffsetDateTime,
}

impl PathMapping {
    /// The rewrite rule alone, for [`crate::remote_path::translate`].
    ///
    /// The matching cares about two prefixes and an id; which client
    /// reported the path is this table's business and not the rule's.
    /// Keeping the two apart is what lets `arr_root_mappings` reuse the
    /// same tested algorithm instead of growing a second copy.
    #[must_use]
    pub fn rule(&self) -> crate::remote_path::PrefixRule {
        crate::remote_path::PrefixRule {
            id: self.id,
            remote_prefix: self.remote_prefix.clone(),
            local_prefix: self.local_prefix.clone(),
        }
    }
}

/// Values used to create a mapping.
#[derive(Debug, Clone, Copy)]
pub struct NewPathMapping<'a> {
    /// Client whose paths this rewrites.
    pub client_id: Uuid,
    /// Remote side, as the operator typed it.
    pub remote_prefix: &'a str,
    /// Local side, as the operator typed it.
    pub local_prefix: &'a str,
}

const COLUMNS: &str = "id, client_id, remote_prefix, local_prefix, created_at";

/// Normalise the local side: trimmed, and without a trailing separator
/// so `/midias/torrents` and `/midias/torrents/` are the same row.
fn normalise_local(raw: &str) -> PathBuf {
    let trimmed = raw.trim();
    let stripped = trimmed
        .strip_suffix(['/', '\\'])
        .filter(|s| !s.is_empty())
        .unwrap_or(trimmed);
    PathBuf::from(stripped)
}

/// Confirm brarr can actually read the local side.
///
/// Mirrors `root_folders::validate`, minus the writability check: brarr
/// only ever reads from a download folder. Requiring write would refuse
/// a perfectly good read-only bind mount of the client's completed
/// directory.
///
/// # Errors
///
/// [`AppError::InvalidInput`] when the path is blank, does not exist, or
/// is not a directory.
fn validate_local(path: &Path) -> Result<(), AppError> {
    if path.as_os_str().is_empty() {
        return Err(AppError::InvalidInput(
            "o caminho no brarr não pode ser vazio".into(),
        ));
    }
    let meta = std::fs::metadata(path).map_err(|e| {
        AppError::InvalidInput(format!(
            "o brarr não consegue acessar {}: {e}. \
             Esse é o caminho como o **brarr** enxerga — confira o bind mount do contêiner dele",
            path.display()
        ))
    })?;
    if !meta.is_dir() {
        return Err(AppError::InvalidInput(format!(
            "{} existe mas não é um diretório",
            path.display()
        )));
    }
    Ok(())
}

fn row_to_mapping(row: &SqliteRow) -> Result<PathMapping, AppError> {
    let id: String = row.try_get("id")?;
    let client_id: String = row.try_get("client_id")?;
    let remote_prefix: String = row.try_get("remote_prefix")?;
    let local_prefix: String = row.try_get("local_prefix")?;
    let created_at: i64 = row.try_get("created_at")?;
    Ok(PathMapping {
        id: Uuid::parse_str(&id)
            .map_err(|e| AppError::InvalidInput(format!("invalid path_mapping id: {e}")))?,
        client_id: Uuid::parse_str(&client_id)
            .map_err(|e| AppError::InvalidInput(format!("invalid client id: {e}")))?,
        remote_prefix,
        local_prefix: PathBuf::from(local_prefix),
        created_at: OffsetDateTime::from_unix_timestamp(created_at)
            .map_err(|e| AppError::InvalidInput(format!("invalid created_at: {e}")))?,
    })
}

/// Register a mapping.
///
/// # Errors
///
/// - [`AppError::NotFound`] when the client does not exist (checked
///   before any syscall, and a 404 instead of the 500 a raw foreign-key
///   violation would give).
/// - [`AppError::InvalidInput`] when the remote side cannot serve as a
///   prefix, when the local side fails [`validate_local`], or when this
///   client already has a mapping for the same prefix.
/// - [`AppError::Database`] on any other SQL failure.
pub async fn insert(pool: &Pool, new: NewPathMapping<'_>) -> Result<PathMapping, AppError> {
    download_clients::get_by_id(pool, new.client_id).await?;

    let remote = crate::remote_path::canonical_prefix(new.remote_prefix).ok_or_else(|| {
        AppError::InvalidInput(
            "o caminho no cliente precisa ser absoluto no formato dele — /data/torrents, \
             C:\\Downloads ou \\\\NAS\\midia — e não pode ser só \"/\", que casaria com tudo"
                .into(),
        )
    })?;
    let local = normalise_local(new.local_prefix);
    validate_local(&local)?;

    let id = Uuid::new_v4();
    let now = OffsetDateTime::now_utc();
    let result = sqlx::query(
        "INSERT INTO path_mappings (id, client_id, remote_prefix, local_prefix, created_at) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(id.to_string())
    .bind(new.client_id.to_string())
    .bind(&remote)
    .bind(local.to_string_lossy().as_ref())
    .bind(now.unix_timestamp())
    .execute(pool)
    .await;

    // `root_folders::insert` lets a UNIQUE violation escape as
    // AppError::Database, which `status_code()` sends to the `_ =>` arm
    // and answers 500 with a SQLite constraint string in the body. A
    // duplicate is form input: 400 and a sentence.
    if let Err(sqlx::Error::Database(db)) = &result {
        if db.is_unique_violation() {
            return Err(AppError::InvalidInput(format!(
                "este cliente já tem um mapeamento para {remote}"
            )));
        }
    }
    result?;

    Ok(PathMapping {
        id,
        client_id: new.client_id,
        remote_prefix: remote,
        local_prefix: local,
        created_at: now,
    })
}

/// Every mapping for one client — what the import reads, once per
/// attempt.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn for_client(pool: &Pool, client_id: Uuid) -> Result<Vec<PathMapping>, AppError> {
    let rows = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM path_mappings WHERE client_id = ? ORDER BY created_at ASC, id ASC"
    ))
    .bind(client_id.to_string())
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_mapping).collect()
}

/// Every mapping, for the admin screen.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn list_all(pool: &Pool) -> Result<Vec<PathMapping>, AppError> {
    let rows = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM path_mappings ORDER BY client_id ASC, remote_prefix ASC"
    ))
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_mapping).collect()
}

/// One row by id.
///
/// # Errors
///
/// Returns [`AppError::NotFound`] when absent, [`AppError::Database`] on
/// SQL failure.
pub async fn get_by_id(pool: &Pool, id: Uuid) -> Result<PathMapping, AppError> {
    let row = sqlx::query(&format!("SELECT {COLUMNS} FROM path_mappings WHERE id = ?"))
        .bind(id.to_string())
        .fetch_optional(pool)
        .await?;
    match row {
        Some(row) => row_to_mapping(&row),
        None => Err(AppError::NotFound(format!("path_mapping {id}"))),
    }
}

/// Remove a mapping. `true` when a row went away.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn delete_by_id(pool: &Pool, id: Uuid) -> Result<bool, AppError> {
    let res = sqlx::query("DELETE FROM path_mappings WHERE id = ?")
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
    use crate::db::download_clients::NewDownloadClient;
    use brarr_download_client::DownloadClientKind;
    use url::Url;

    /// A directory that is guaranteed to exist, for the local side.
    /// `std::env::temp_dir()` is always present, which is all
    /// [`validate_local`] asks for — no need for a fixture crate.
    fn existing_dir() -> String {
        std::env::temp_dir().to_string_lossy().into_owned()
    }

    async fn client(pool: &Pool, name: &str) -> Uuid {
        let url = Url::parse("http://127.0.0.1:8080/").unwrap();
        download_clients::insert(
            pool,
            NewDownloadClient {
                name,
                kind: DownloadClientKind::Qbittorrent,
                base_url: &url,
                username: None,
                password: None,
                api_key: None,
                category: None,
                priority: None,
                enabled: None,
            },
        )
        .await
        .unwrap()
        .id
    }

    #[tokio::test]
    async fn insert_canonicalises_the_remote_side_and_keeps_the_local_one() {
        let pool = crate::db::open_memory().await.unwrap();
        let id = client(&pool, "qb").await;
        let local = existing_dir();

        let row = insert(
            &pool,
            NewPathMapping {
                client_id: id,
                // Trailing separator, doubled separator, and a `.`.
                remote_prefix: "/data//torrents/./",
                local_prefix: &local,
            },
        )
        .await
        .unwrap();

        assert_eq!(row.remote_prefix, "/data/torrents");
        assert_eq!(row.local_prefix, normalise_local(&local));
    }

    #[tokio::test]
    async fn a_relative_remote_prefix_is_refused() {
        let pool = crate::db::open_memory().await.unwrap();
        let id = client(&pool, "qb").await;

        let err = insert(
            &pool,
            NewPathMapping {
                client_id: id,
                remote_prefix: "torrents",
                local_prefix: &existing_dir(),
            },
        )
        .await
        .unwrap_err();

        assert!(matches!(err, AppError::InvalidInput(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn the_bare_posix_root_is_refused_because_it_would_match_everything() {
        let pool = crate::db::open_memory().await.unwrap();
        let id = client(&pool, "qb").await;

        let err = insert(
            &pool,
            NewPathMapping {
                client_id: id,
                remote_prefix: "/",
                local_prefix: &existing_dir(),
            },
        )
        .await
        .unwrap_err();

        assert!(matches!(err, AppError::InvalidInput(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn a_local_side_that_does_not_exist_is_a_form_error_not_a_500() {
        let pool = crate::db::open_memory().await.unwrap();
        let id = client(&pool, "qb").await;

        let err = insert(
            &pool,
            NewPathMapping {
                client_id: id,
                remote_prefix: "/data/torrents",
                local_prefix: "/caminho/que/nao/existe/em/lugar/nenhum",
            },
        )
        .await
        .unwrap_err();

        assert!(matches!(err, AppError::InvalidInput(_)), "got {err:?}");
        assert_eq!(err.status_code(), axum::http::StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_duplicate_prefix_is_a_form_error_not_a_500() {
        let pool = crate::db::open_memory().await.unwrap();
        let id = client(&pool, "qb").await;
        let local = existing_dir();
        let new = NewPathMapping {
            client_id: id,
            remote_prefix: "/data/torrents",
            local_prefix: &local,
        };
        insert(&pool, new).await.unwrap();

        // Same prefix, spelled differently — canonicalisation makes them
        // collide, which is the point.
        let err = insert(
            &pool,
            NewPathMapping {
                client_id: id,
                remote_prefix: "/data/torrents/",
                local_prefix: &local,
            },
        )
        .await
        .unwrap_err();

        assert!(matches!(err, AppError::InvalidInput(_)), "got {err:?}");
        assert_eq!(err.status_code(), axum::http::StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn an_unknown_client_is_not_found() {
        let pool = crate::db::open_memory().await.unwrap();

        let err = insert(
            &pool,
            NewPathMapping {
                client_id: Uuid::new_v4(),
                remote_prefix: "/data/torrents",
                local_prefix: &existing_dir(),
            },
        )
        .await
        .unwrap_err();

        assert!(matches!(err, AppError::NotFound(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn deleting_the_client_cascades_to_its_mappings() {
        let pool = crate::db::open_memory().await.unwrap();
        let id = client(&pool, "qb").await;
        insert(
            &pool,
            NewPathMapping {
                client_id: id,
                remote_prefix: "/data/torrents",
                local_prefix: &existing_dir(),
            },
        )
        .await
        .unwrap();
        assert_eq!(for_client(&pool, id).await.unwrap().len(), 1);

        download_clients::delete_by_id(&pool, id).await.unwrap();

        assert!(
            for_client(&pool, id).await.unwrap().is_empty(),
            "a mapping without a client is dead configuration"
        );
    }
}
