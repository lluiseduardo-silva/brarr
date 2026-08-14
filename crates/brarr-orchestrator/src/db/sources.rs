//! `metadata_sources` — the registry every other `source` column points
//! at.
//!
//! One row per [`MetadataSource`], seeded by the migration and reconciled
//! at boot. Reconciling rather than trusting the seed is the whole point:
//! a source added to the enum after this migration shipped would have no
//! row, and its first write would die on the foreign key. [`ensure`] runs
//! before the workers start, so a variant added in Rust is a row here by
//! the time anything can reference it.
//!
//! That is the same defect class as `'tvdb'` missing from a CHECK — valid
//! in Rust, inert in SQLite, suite green — with the fix moved to where it
//! can be automatic. A CHECK could not be reconciled at boot; a table can.

use brarr_core::{MetadataSource, SourceKind};
use sqlx::Row;

use crate::{AppError, db::Pool};

/// Bring the registry in line with the enum.
///
/// Idempotent, and additive only: a label the enum no longer carries is
/// left alone rather than deleted, because rows in `library_item_ids`
/// still point at it and losing them to a rename would be worse than a
/// stale row nothing reads.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn ensure(pool: &Pool) -> Result<(), AppError> {
    for source in MetadataSource::all() {
        sqlx::query(
            "INSERT INTO metadata_sources (label, display_name, kind) VALUES (?, ?, ?) \
             ON CONFLICT(label) DO UPDATE SET \
                display_name = excluded.display_name, kind = excluded.kind",
        )
        .bind(source.label())
        .bind(source.display_name())
        .bind(kind_label(source.kind()))
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Every label the database holds, whether or not the enum still knows
/// it. Only the guards read this.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn labels(pool: &Pool) -> Result<Vec<String>, AppError> {
    let rows = sqlx::query("SELECT label FROM metadata_sources ORDER BY label")
        .fetch_all(pool)
        .await?;
    rows.iter().map(|r| Ok(r.try_get("label")?)).collect()
}

/// The `kind` column's spelling.
const fn kind_label(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::Provider => "provider",
        SourceKind::Namespace => "namespace",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;
    use crate::db::open_memory;

    /// **The guard the `'tvdb'` incident asked for**, one level up.
    ///
    /// There is no CHECK to fall out of sync with any more, but there is
    /// a foreign key — and a source with no row here fails every write
    /// that references it. Walking the real enum against a migrated
    /// database is what makes a new variant fail in this test rather than
    /// in a log file.
    #[tokio::test]
    async fn every_source_has_a_row() {
        let pool = open_memory().await.unwrap();
        let stored = labels(&pool).await.unwrap();
        for source in MetadataSource::all() {
            assert!(
                stored.iter().any(|l| l == source.label()),
                "{source} has no row in metadata_sources"
            );
        }
    }

    /// And a variant the migration never seeded is seeded at boot, so the
    /// gap between "added to the enum" and "usable" is one function call
    /// rather than one migration.
    #[tokio::test]
    async fn ensure_is_idempotent_and_fills_a_gap() {
        let pool = open_memory().await.unwrap();
        sqlx::query("DELETE FROM metadata_sources WHERE label = 'tvdb'")
            .execute(&pool)
            .await
            .unwrap();
        assert!(!labels(&pool).await.unwrap().iter().any(|l| l == "tvdb"));

        ensure(&pool).await.unwrap();
        ensure(&pool).await.unwrap();

        let stored = labels(&pool).await.unwrap();
        assert_eq!(
            stored.len(),
            MetadataSource::all().count(),
            "ensure duplicated or dropped a row"
        );
        assert!(stored.iter().any(|l| l == "tvdb"));
    }

    /// The `kind` column carries a CHECK, so every value the enum can
    /// produce has to be one it accepts — the failure this whole design
    /// moved off provider names, still live for brarr's own vocabulary.
    #[tokio::test]
    async fn every_kind_survives_the_check() {
        let pool = open_memory().await.unwrap();
        ensure(&pool).await.unwrap();
        for source in MetadataSource::all() {
            let stored: String = sqlx::query("SELECT kind FROM metadata_sources WHERE label = ?")
                .bind(source.label())
                .fetch_one(&pool)
                .await
                .unwrap()
                .try_get("kind")
                .unwrap();
            assert_eq!(stored, kind_label(source.kind()), "{source}");
        }
    }
}
