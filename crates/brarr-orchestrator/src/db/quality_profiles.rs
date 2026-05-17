//! `quality_profiles` table — reusable scoring presets that an *arr
//! instance can attach to instead of carrying a bare integer
//! threshold.
//!
//! See `migrations/20260518120000_quality_profiles.sql` for the
//! schema rationale (TL;DR: profile = "named threshold + future home
//! for a custom rule list"). MVP exposes name / description /
//! threshold; rule storage is a follow-up phase.

use sqlx::{Row, sqlite::SqliteRow};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{AppError, db::Pool};

/// One quality-profile row.
#[derive(Debug, Clone)]
pub struct QualityProfileRow {
    /// Stable UUID v4 used in URLs and `arr_instances.profile_id`.
    pub id: Uuid,
    /// Operator-facing name. Unique across rows.
    pub name: String,
    /// Optional one-line description.
    pub description: Option<String>,
    /// Minimum [`brarr_core::DecisionScore`] required to auto-push
    /// when this profile is attached.
    pub push_threshold: u32,
    /// `true` for the rows we seed in the migration so the UI can
    /// badge them as PRESET. Edits / deletes are still allowed; the
    /// flag is purely a hint.
    pub is_preset: bool,
    /// Row creation timestamp.
    pub created_at: OffsetDateTime,
}

/// Bundle of values used to create a new profile.
#[derive(Debug, Clone)]
pub struct NewQualityProfile<'a> {
    /// Operator-chosen name. Must be unique. Whitespace-only rejected.
    pub name: &'a str,
    /// Optional description.
    pub description: Option<&'a str>,
    /// Threshold in `0..=1000`.
    pub push_threshold: u32,
}

/// Insert a new profile, returning the persisted row.
///
/// # Errors
///
/// - [`AppError::InvalidInput`] when `name` is blank or `threshold > 1000`.
/// - [`AppError::Database`] on `UNIQUE(name)` violation or other SQL error.
pub async fn insert(
    pool: &Pool,
    new: NewQualityProfile<'_>,
) -> Result<QualityProfileRow, AppError> {
    if new.name.trim().is_empty() {
        return Err(AppError::InvalidInput(
            "quality profile name cannot be empty".into(),
        ));
    }
    if new.push_threshold > 1000 {
        return Err(AppError::InvalidInput(format!(
            "push_threshold must be 0..=1000, got {}",
            new.push_threshold
        )));
    }
    let id = Uuid::new_v4();
    let now = OffsetDateTime::now_utc();
    sqlx::query(
        "INSERT INTO quality_profiles (id, name, description, push_threshold, is_preset, created_at) \
         VALUES (?, ?, ?, ?, 0, ?)",
    )
    .bind(id.to_string())
    .bind(new.name.trim())
    .bind(new.description)
    .bind(i64::from(new.push_threshold))
    .bind(now.unix_timestamp())
    .execute(pool)
    .await?;
    Ok(QualityProfileRow {
        id,
        name: new.name.trim().to_string(),
        description: new.description.map(str::to_string),
        push_threshold: new.push_threshold,
        is_preset: false,
        created_at: now,
    })
}

/// List every profile ordered by `is_preset DESC, name ASC` so the
/// seeded presets bubble to the top.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn list_all(pool: &Pool) -> Result<Vec<QualityProfileRow>, AppError> {
    let rows = sqlx::query(
        "SELECT id, name, description, push_threshold, is_preset, created_at \
         FROM quality_profiles ORDER BY is_preset DESC, name ASC",
    )
    .fetch_all(pool)
    .await?;
    rows.iter().map(row_to_profile).collect()
}

/// Fetch one profile by id.
///
/// # Errors
///
/// Returns [`AppError::NotFound`] when no row matches.
pub async fn get_by_id(pool: &Pool, id: Uuid) -> Result<QualityProfileRow, AppError> {
    let row_opt = sqlx::query(
        "SELECT id, name, description, push_threshold, is_preset, created_at \
         FROM quality_profiles WHERE id = ?",
    )
    .bind(id.to_string())
    .fetch_optional(pool)
    .await?;
    match row_opt {
        Some(row) => row_to_profile(&row),
        None => Err(AppError::NotFound(format!("quality_profile {id}"))),
    }
}

/// Delete a profile. Returns `true` if a row was removed. Any
/// `arr_instances.profile_id` pointing at it is set to NULL by the
/// `ON DELETE SET NULL` clause in the migration.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn delete_by_id(pool: &Pool, id: Uuid) -> Result<bool, AppError> {
    let res = sqlx::query("DELETE FROM quality_profiles WHERE id = ?")
        .bind(id.to_string())
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

fn row_to_profile(row: &SqliteRow) -> Result<QualityProfileRow, AppError> {
    let id_str: String = row.try_get("id")?;
    let id = Uuid::parse_str(&id_str)
        .map_err(|e| AppError::InvalidInput(format!("invalid uuid in quality_profiles.id: {e}")))?;
    let threshold_i64: i64 = row.try_get("push_threshold")?;
    let push_threshold = u32::try_from(threshold_i64).unwrap_or(0);
    let is_preset_i64: i64 = row.try_get("is_preset")?;
    let created_unix: i64 = row.try_get("created_at")?;
    let created_at = OffsetDateTime::from_unix_timestamp(created_unix)
        .map_err(|e| AppError::InvalidInput(format!("invalid timestamp: {e}")))?;
    Ok(QualityProfileRow {
        id,
        name: row.try_get("name")?,
        description: row.try_get("description")?,
        push_threshold,
        is_preset: is_preset_i64 != 0,
        created_at,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::db::open_memory;

    #[tokio::test]
    async fn presets_seeded_by_migration() {
        let pool = open_memory().await.unwrap();
        let all = list_all(&pool).await.unwrap();
        assert_eq!(all.len(), 5, "5 presets ship with the migration");
        assert!(all.iter().all(|p| p.is_preset));
        let names: Vec<_> = all.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"FHD Dublado"));
        assert!(names.contains(&"4K HDR Dublado"));
    }

    #[tokio::test]
    async fn insert_and_list_includes_new_row() {
        let pool = open_memory().await.unwrap();
        let row = insert(
            &pool,
            NewQualityProfile {
                name: "anime jp",
                description: Some("Áudio japonês + legenda PT"),
                push_threshold: 90,
            },
        )
        .await
        .unwrap();
        assert_eq!(row.push_threshold, 90);
        assert!(!row.is_preset);
        let all = list_all(&pool).await.unwrap();
        assert_eq!(all.len(), 6);
        // Presets appear first (is_preset DESC).
        assert!(all[0].is_preset);
    }

    #[tokio::test]
    async fn duplicate_name_violates_unique() {
        let pool = open_memory().await.unwrap();
        let _ = insert(
            &pool,
            NewQualityProfile {
                name: "dupe",
                description: None,
                push_threshold: 100,
            },
        )
        .await
        .unwrap();
        let err = insert(
            &pool,
            NewQualityProfile {
                name: "dupe",
                description: None,
                push_threshold: 200,
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::Database(_)));
    }

    #[tokio::test]
    async fn blank_name_rejected() {
        let pool = open_memory().await.unwrap();
        let err = insert(
            &pool,
            NewQualityProfile {
                name: "  ",
                description: None,
                push_threshold: 100,
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn threshold_above_1000_rejected() {
        let pool = open_memory().await.unwrap();
        let err = insert(
            &pool,
            NewQualityProfile {
                name: "over",
                description: None,
                push_threshold: 1500,
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn get_by_id_404s_when_missing() {
        let pool = open_memory().await.unwrap();
        let err = get_by_id(&pool, Uuid::new_v4()).await.unwrap_err();
        assert!(matches!(err, AppError::NotFound(_)));
    }

    #[tokio::test]
    async fn delete_returns_true_only_when_row_existed() {
        let pool = open_memory().await.unwrap();
        let row = insert(
            &pool,
            NewQualityProfile {
                name: "x",
                description: None,
                push_threshold: 100,
            },
        )
        .await
        .unwrap();
        assert!(delete_by_id(&pool, row.id).await.unwrap());
        assert!(!delete_by_id(&pool, row.id).await.unwrap());
    }
}
