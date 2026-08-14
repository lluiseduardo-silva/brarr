//! `scan_attempts` — when the sweep last searched for each target.
//!
//! The one piece of state that makes [`crate::scan`]'s per-cycle ceiling
//! safe. Without it the budget is spent from a fixed head and the tail of
//! the wanted list is never reached at all; see
//! `migrations/20260814170000_scan_rotation.sql` for the measurement that
//! produced this table.
//!
//! Deliberately not derived from `searches`: that table is pruned on the
//! retention window, so a target would read as never-searched the moment
//! its history aged out — and the Torznab pull path writes rows there that
//! no sweep produced.

use std::collections::HashMap;

use sqlx::Row;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{AppError, db::Pool};

/// Which target an attempt is about: the item, and the episode when it
/// names one.
///
/// A pair rather than "whichever id is set", because the two halves are
/// not interchangeable — a movie's target *is* the item, and a series is
/// only ever searched per episode.
pub type Key = (Uuid, Option<Uuid>);

/// Every recorded attempt, for the whole library, in one query.
///
/// A miss means never searched, which is the answer the sweep wants: an
/// unsearched target sorts ahead of every searched one.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn last_searched(pool: &Pool) -> Result<HashMap<Key, OffsetDateTime>, AppError> {
    let rows = sqlx::query("SELECT item_id, episode_id, searched_at FROM scan_attempts")
        .fetch_all(pool)
        .await?;
    let mut out = HashMap::with_capacity(rows.len());
    for row in &rows {
        let item: String = row.try_get("item_id")?;
        let episode: Option<String> = row.try_get("episode_id")?;
        let at: i64 = row.try_get("searched_at")?;
        let item_id = Uuid::parse_str(&item)
            .map_err(|e| AppError::InvalidInput(format!("invalid scan_attempts.item_id: {e}")))?;
        let episode_id = episode
            .as_deref()
            .map(Uuid::parse_str)
            .transpose()
            .map_err(|e| {
                AppError::InvalidInput(format!("invalid scan_attempts.episode_id: {e}"))
            })?;
        // A stored value outside the representable range is bad data, not
        // a reason to fail the read — the same degradation `library`
        // applies to its timestamps. Dropping the row reads as "never
        // searched", which searches it again rather than skipping it.
        if let Ok(searched_at) = OffsetDateTime::from_unix_timestamp(at) {
            out.insert((item_id, episode_id), searched_at);
        }
    }
    Ok(out)
}

/// Stamp a target as searched now.
///
/// `INSERT OR REPLACE` because the row is one value keyed by target and
/// the two partial unique indexes are two different conflict targets — an
/// `ON CONFLICT` clause would have to name one and would miss the other.
/// Nothing references this table, so the delete half of a replace costs
/// nothing.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn record(
    pool: &Pool,
    item_id: Uuid,
    episode_id: Option<Uuid>,
    at: OffsetDateTime,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT OR REPLACE INTO scan_attempts (item_id, episode_id, searched_at) \
         VALUES (?, ?, ?)",
    )
    .bind(item_id.to_string())
    .bind(episode_id.map(|id| id.to_string()))
    .bind(at.unix_timestamp())
    .execute(pool)
    .await?;
    Ok(())
}
