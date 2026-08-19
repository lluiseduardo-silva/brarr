//! `media_server_path_mappings` — what brarr calls a directory, and what
//! the media server calls the same place.
//!
//! The third table feeding [`crate::remote_path::PrefixRule`], after
//! `path_mappings` (a download client's namespace) and
//! `arr_root_mappings` (a Sonarr/Radarr root). The module header over
//! there says a second table copying the algorithm is how the incident
//! that produced it comes back, and that is why this one stores rows and
//! nothing else: the matching is [`crate::remote_path::to_remote`].
//!
//! The direction is the only difference, and it does not change the
//! vocabulary. `remote_prefix` is still "what the *other* side writes"
//! and `local_prefix` is still brarr's — the import reads a client's
//! path inward, this writes brarr's path outward, and both are answering
//! "what does the other side call this directory?".
//!
//! Same two conventions as `path_mappings`: only the local side is
//! validated against this filesystem (the remote side names a directory
//! on another machine and *should* not exist here), and there is no
//! update path, which keeps the stored prefix canonical by construction.

use std::path::{Path, PathBuf};

use sqlx::{Row, sqlite::SqliteRow};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::db::media_servers;
use crate::{AppError, db::Pool};

/// One local → remote prefix rewrite, for one media server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaServerMapping {
    /// Stable UUID v4, used in the delete route.
    pub id: Uuid,
    /// Which server sees paths in this namespace.
    pub server_id: Uuid,
    /// Prefix as the *media server* writes it. A `String`, not a
    /// `PathBuf`: it is not a path on this machine.
    pub remote_prefix: String,
    /// Prefix in brarr's namespace. A `PathBuf`, because it is one.
    pub local_prefix: PathBuf,
    /// Row creation timestamp.
    pub created_at: OffsetDateTime,
}

impl MediaServerMapping {
    /// The rewrite rule alone, for [`crate::remote_path::to_remote`].
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
pub struct NewMediaServerMapping<'a> {
    /// Server whose namespace this writes into.
    pub server_id: Uuid,
    /// Remote side, as the operator typed it.
    pub remote_prefix: &'a str,
    /// Local side, as the operator typed it.
    pub local_prefix: &'a str,
}

const COLUMNS: &str = "id, server_id, remote_prefix, local_prefix, created_at";

/// Normalise the local side: trimmed, and without a trailing separator.
fn normalise_local(raw: &str) -> PathBuf {
    let trimmed = raw.trim();
    let stripped = trimmed
        .strip_suffix(['/', '\\'])
        .filter(|s| !s.is_empty())
        .unwrap_or(trimmed);
    PathBuf::from(stripped)
}

/// Confirm brarr can actually see the local side.
///
/// Mirrors `path_mappings::validate_local`. Read-only is enough: brarr
/// never writes through a mapping, it only names paths it already wrote.
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

fn row_to_mapping(row: &SqliteRow) -> Result<MediaServerMapping, AppError> {
    let id: String = row.try_get("id")?;
    let server_id: String = row.try_get("server_id")?;
    let local_prefix: String = row.try_get("local_prefix")?;
    let created_at: i64 = row.try_get("created_at")?;
    Ok(MediaServerMapping {
        id: Uuid::parse_str(&id)
            .map_err(|e| AppError::InvalidInput(format!("invalid mapping id: {e}")))?,
        server_id: Uuid::parse_str(&server_id)
            .map_err(|e| AppError::InvalidInput(format!("invalid server id: {e}")))?,
        remote_prefix: row.try_get("remote_prefix")?,
        local_prefix: PathBuf::from(local_prefix),
        created_at: OffsetDateTime::from_unix_timestamp(created_at)
            .map_err(|e| AppError::InvalidInput(format!("invalid created_at: {e}")))?,
    })
}

/// Register a mapping.
///
/// # Errors
///
/// - [`AppError::NotFound`] when the server does not exist.
/// - [`AppError::InvalidInput`] when the remote side cannot serve as a
///   prefix, when the local side fails [`validate_local`], or when this
///   server already has a mapping for the same prefix.
/// - [`AppError::Database`] on any other SQL failure.
pub async fn insert(
    pool: &Pool,
    new: NewMediaServerMapping<'_>,
) -> Result<MediaServerMapping, AppError> {
    media_servers::get_by_id(pool, new.server_id).await?;

    let remote = crate::remote_path::canonical_prefix(new.remote_prefix).ok_or_else(|| {
        AppError::InvalidInput(
            "o caminho no media server precisa ser absoluto no formato dele — /mnt/midias, \
             C:\\Media ou \\\\NAS\\midia — e não pode ser só \"/\", que casaria com tudo"
                .into(),
        )
    })?;
    let local = normalise_local(new.local_prefix);
    validate_local(&local)?;

    let id = Uuid::new_v4();
    let now = OffsetDateTime::now_utc();
    let result = sqlx::query(
        "INSERT INTO media_server_path_mappings \
         (id, server_id, remote_prefix, local_prefix, created_at) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(id.to_string())
    .bind(new.server_id.to_string())
    .bind(&remote)
    .bind(local.to_string_lossy().as_ref())
    .bind(now.unix_timestamp())
    .execute(pool)
    .await;

    if let Err(sqlx::Error::Database(db)) = &result {
        if db.is_unique_violation() {
            return Err(AppError::InvalidInput(format!(
                "este servidor já tem um mapeamento para {remote}"
            )));
        }
    }
    result?;

    Ok(MediaServerMapping {
        id,
        server_id: new.server_id,
        remote_prefix: remote,
        local_prefix: local,
        created_at: now,
    })
}

/// Every mapping for one server — what the notify path reads.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn for_server(pool: &Pool, server_id: Uuid) -> Result<Vec<MediaServerMapping>, AppError> {
    let rows = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM media_server_path_mappings WHERE server_id = ? \
         ORDER BY created_at ASC, id ASC"
    ))
    .bind(server_id.to_string())
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_mapping).collect()
}

/// The rules for one server, ready for [`crate::remote_path::to_remote`].
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn rules_for_server(
    pool: &Pool,
    server_id: Uuid,
) -> Result<Vec<crate::remote_path::PrefixRule>, AppError> {
    Ok(for_server(pool, server_id)
        .await?
        .iter()
        .map(MediaServerMapping::rule)
        .collect())
}

/// Every mapping, for the admin screen.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn list_all(pool: &Pool) -> Result<Vec<MediaServerMapping>, AppError> {
    let rows = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM media_server_path_mappings \
         ORDER BY server_id ASC, remote_prefix ASC"
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
    let res = sqlx::query("DELETE FROM media_server_path_mappings WHERE id = ?")
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
    use crate::db;
    use crate::db::media_servers::{MediaServerRow, NewMediaServer};
    use brarr_media_server::MediaServerKind;

    async fn pool() -> Pool {
        db::open(":memory:").await.expect("in-memory db")
    }

    async fn server(pool: &Pool) -> MediaServerRow {
        media_servers::insert(
            pool,
            NewMediaServer {
                name: "Plex",
                kind: MediaServerKind::Plex,
                base_url: "http://10.0.1.248:32400",
                token: Some("t"),
            },
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn a_mapping_produces_a_rule_the_shared_matcher_understands() {
        let pool = pool().await;
        let server = server(&pool).await;
        let local = std::env::temp_dir();

        let mapping = insert(
            &pool,
            NewMediaServerMapping {
                server_id: server.id,
                remote_prefix: "/mnt/midias/",
                local_prefix: &local.to_string_lossy(),
            },
        )
        .await
        .unwrap();

        assert_eq!(
            mapping.remote_prefix, "/mnt/midias",
            "stored canonical, so the matching never compares a half-normalised string"
        );
        let rule = mapping.rule();
        assert_eq!(rule.id, mapping.id);
        assert_eq!(rule.remote_prefix, "/mnt/midias");
    }

    #[tokio::test]
    async fn the_remote_side_is_never_checked_against_this_filesystem() {
        let pool = pool().await;
        let server = server(&pool).await;
        // `/mnt/midias` does not exist on the machine running this test,
        // and that is exactly the situation the row exists for.
        insert(
            &pool,
            NewMediaServerMapping {
                server_id: server.id,
                remote_prefix: "/mnt/midias",
                local_prefix: &std::env::temp_dir().to_string_lossy(),
            },
        )
        .await
        .expect("the other machine's path is not this machine's business");
    }

    #[tokio::test]
    async fn the_local_side_is_checked_at_registration() {
        let pool = pool().await;
        let server = server(&pool).await;
        let err = insert(
            &pool,
            NewMediaServerMapping {
                server_id: server.id,
                remote_prefix: "/mnt/midias",
                local_prefix: "/nao/existe/em/lugar/nenhum",
            },
        )
        .await
        .expect_err("a typo caught now is a form error, not a silent no-op later");
        assert!(matches!(err, AppError::InvalidInput(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn a_bare_root_is_refused_because_it_would_cover_everything() {
        let pool = pool().await;
        let server = server(&pool).await;
        let err = insert(
            &pool,
            NewMediaServerMapping {
                server_id: server.id,
                remote_prefix: "/",
                local_prefix: &std::env::temp_dir().to_string_lossy(),
            },
        )
        .await
        .expect_err("would rewrite every path");
        assert!(matches!(err, AppError::InvalidInput(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn removing_the_server_removes_its_mappings() {
        let pool = pool().await;
        let server = server(&pool).await;
        insert(
            &pool,
            NewMediaServerMapping {
                server_id: server.id,
                remote_prefix: "/mnt/midias",
                local_prefix: &std::env::temp_dir().to_string_lossy(),
            },
        )
        .await
        .unwrap();

        media_servers::delete_by_id(&pool, server.id).await.unwrap();
        assert!(
            list_all(&pool).await.unwrap().is_empty(),
            "a mapping without a server is dead configuration, unlike a grab without a client"
        );
    }
}
