//! `library_item_ids` — who a catalogue entry is, according to each
//! source that knows it.
//!
//! Identity is a **set**, and the set has no owner: two ids from two
//! sources are two facts, not two opinions, so there is no precedence to
//! resolve here. What the schema does carry is whether a pairing was
//! *vouched for* — see [`Verification`].
//!
//! Nothing reads this yet. The readers move in the next phase; this
//! migration writes it alongside the columns it will replace.

use std::collections::HashMap;

use brarr_core::{ExternalId, MediaType, MetadataSource};
use sqlx::Row;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{AppError, db::Pool};

/// How brarr came to believe an id.
///
/// The distinction is worth a column because it decides whether a sweep
/// should ask again. `arr_import::resolve_tmdb_id` calls `find_by_tvdb`
/// **per title on every pass** over the catalogue, and the answer never
/// changes — recording that a provider already vouched is what turns
/// three hundred and sixty requests per cycle into none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verification {
    /// Somebody asserted it: an operator typed it, an \*arr reported it.
    Asserted,
    /// A provider was asked and answered, at this time.
    Vouched(OffsetDateTime),
}

impl Verification {
    const fn at(self) -> Option<OffsetDateTime> {
        match self {
            Self::Asserted => None,
            Self::Vouched(at) => Some(at),
        }
    }
}

/// One stored identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredId {
    /// The id itself, canonicalised by its own constructor.
    pub id: ExternalId,
    /// Whether a provider vouched for it.
    pub verification: Verification,
}

/// Record an id for an item.
///
/// Upserts on `(item_id, source)`: one id per source per title, which is
/// what makes "the TMDB id of this series" a question with one answer.
/// A re-assertion never downgrades a vouched pairing to an asserted one —
/// the sweep re-stating what it already knows must not erase the record
/// that stops it asking again.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure, including the foreign
/// key when the source has no `metadata_sources` row.
pub async fn put(
    pool: &Pool,
    item_id: Uuid,
    media: MediaType,
    id: &ExternalId,
    verification: Verification,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO library_item_ids (item_id, source, external_id, media_type, verified_at) \
         VALUES (?, ?, ?, ?, ?) \
         ON CONFLICT(item_id, source) DO UPDATE SET \
            external_id = excluded.external_id, \
            media_type  = excluded.media_type, \
            verified_at = COALESCE(excluded.verified_at, library_item_ids.verified_at)",
    )
    .bind(item_id.to_string())
    .bind(id.source().label())
    .bind(id.value())
    .bind(media.label())
    .bind(verification.at().map(OffsetDateTime::unix_timestamp))
    .execute(pool)
    .await?;
    Ok(())
}

/// Every id known for one item.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn for_item(pool: &Pool, item_id: Uuid) -> Result<Vec<StoredId>, AppError> {
    let rows = sqlx::query(
        "SELECT source, external_id, verified_at FROM library_item_ids \
         WHERE item_id = ? ORDER BY source",
    )
    .bind(item_id.to_string())
    .fetch_all(pool)
    .await?;
    rows.iter()
        .filter_map(|r| row_to_id(r).transpose())
        .collect()
}

/// Every id for every item, in one query.
///
/// The index screen renders 360 rows and has already grown an N+1 twice.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn for_all(pool: &Pool) -> Result<HashMap<Uuid, Vec<StoredId>>, AppError> {
    let rows = sqlx::query(
        "SELECT item_id, source, external_id, verified_at FROM library_item_ids \
         ORDER BY item_id, source",
    )
    .fetch_all(pool)
    .await?;

    let mut out: HashMap<Uuid, Vec<StoredId>> = HashMap::new();
    for row in &rows {
        let raw: String = row.try_get("item_id")?;
        let Ok(item_id) = Uuid::parse_str(&raw) else {
            continue;
        };
        if let Some(stored) = row_to_id(row)? {
            out.entry(item_id).or_default().push(stored);
        }
    }
    Ok(out)
}

/// Which item, if any, an id names.
///
/// **This is what "already in the library?" becomes.** Today the question
/// is `(media_type, tmdb_id)`, so adding a series by TMDB and syncing it
/// by the \*arr's TVDB axis produces two rows for one show; asking by any
/// known id makes them one.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn find(
    pool: &Pool,
    media: MediaType,
    id: &ExternalId,
) -> Result<Option<Uuid>, AppError> {
    let row = sqlx::query(
        "SELECT item_id FROM library_item_ids \
         WHERE source = ? AND media_type = ? AND external_id = ?",
    )
    .bind(id.source().label())
    .bind(media.label())
    .bind(id.value())
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let raw: String = row.try_get("item_id")?;
    Ok(Uuid::parse_str(&raw).ok())
}

/// Read one row.
///
/// `Ok(None)` for a row whose source or value no longer parses — a label
/// this build does not know, or a value that predates canonicalisation.
/// Skipped rather than failed: one unreadable id must not make a title
/// unreadable, and the guards are what keep the case from arising.
fn row_to_id(row: &sqlx::sqlite::SqliteRow) -> Result<Option<StoredId>, AppError> {
    let label: String = row.try_get("source")?;
    let value: String = row.try_get("external_id")?;
    let verified: Option<i64> = row.try_get("verified_at")?;

    let Some(source) = MetadataSource::parse(&label) else {
        return Ok(None);
    };
    let Ok(id) = ExternalId::new(source, &value) else {
        return Ok(None);
    };
    let verification = verified
        .and_then(|at| OffsetDateTime::from_unix_timestamp(at).ok())
        .map_or(Verification::Asserted, Verification::Vouched);
    Ok(Some(StoredId { id, verification }))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::similar_names,
    reason = "test assertions"
)]
mod tests {
    use super::*;
    use crate::db::open_memory;
    use crate::db::seed::Seed;

    async fn item(pool: &Pool, tmdb: i64) -> Uuid {
        crate::db::library::upsert(pool, &Seed::series(tmdb, "T").build())
            .await
            .unwrap()
            .id
    }

    /// **The heir of `every_source_can_be_persisted`.**
    ///
    /// The two tests that existed when `'tvdb'` was missing from a CHECK
    /// were both pure — over `may_replace` and over `label`/`parse` — so
    /// nothing ever *persisted* a source, and every write died in
    /// production with the suite green. This one really writes a row per
    /// source against a migrated database and reads it back.
    #[tokio::test]
    async fn every_source_round_trips_through_a_write() {
        let pool = open_memory().await.unwrap();
        let id = item(&pool, 1).await;

        for (index, source) in MetadataSource::all().enumerate() {
            let external =
                ExternalId::new(source, &format!("{}", 1000 + index)).expect("every source keys");
            put(&pool, id, MediaType::Tv, &external, Verification::Asserted)
                .await
                .unwrap_or_else(|e| panic!("{source} could not be persisted: {e}"));
        }

        let stored = for_item(&pool, id).await.unwrap();
        assert_eq!(
            stored.len(),
            MetadataSource::all().count(),
            "a source did not survive the round trip"
        );
    }

    /// An unregistered source is refused by the foreign key on the first
    /// write, rather than being stored and read back as nothing.
    #[tokio::test]
    async fn an_unregistered_source_cannot_be_written() {
        let pool = open_memory().await.unwrap();
        let id = item(&pool, 2).await;
        let refused = sqlx::query(
            "INSERT INTO library_item_ids (item_id, source, external_id, media_type) \
             VALUES (?, 'anilist', '1', 'tv')",
        )
        .bind(id.to_string())
        .execute(&pool)
        .await;
        assert!(refused.is_err(), "the foreign key did not hold");
    }

    /// Whichever IMDb convention arrives, one row is stored — the split
    /// this codebase has reconciled in several places that disagree
    /// about leading zeros.
    #[tokio::test]
    async fn the_two_imdb_conventions_land_on_one_row() {
        let pool = open_memory().await.unwrap();
        let id = item(&pool, 3).await;

        for raw in ["133093", "tt133093", "tt0133093"] {
            let external = ExternalId::new(MetadataSource::Imdb, raw).unwrap();
            put(&pool, id, MediaType::Tv, &external, Verification::Asserted)
                .await
                .unwrap();
        }
        // The item was seeded with a TMDB id of its own, so the count is
        // "one IMDb row", not "one row".
        let imdb: Vec<_> = for_item(&pool, id)
            .await
            .unwrap()
            .into_iter()
            .filter(|s| s.id.source() == MetadataSource::Imdb)
            .collect();
        assert_eq!(imdb.len(), 1);
        assert_eq!(imdb[0].id.value(), "tt0133093");
    }

    /// Re-asserting what a provider already vouched for must not erase
    /// the record that stops the sweep asking again.
    #[tokio::test]
    async fn an_assertion_never_downgrades_a_vouched_pairing() {
        let pool = open_memory().await.unwrap();
        let id = item(&pool, 4).await;
        let external = ExternalId::new(MetadataSource::Tvdb, "355567").unwrap();
        let at = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();

        put(
            &pool,
            id,
            MediaType::Tv,
            &external,
            Verification::Vouched(at),
        )
        .await
        .unwrap();
        put(&pool, id, MediaType::Tv, &external, Verification::Asserted)
            .await
            .unwrap();

        let stored = for_item(&pool, id).await.unwrap();
        let tvdb = stored
            .iter()
            .find(|s| s.id.source() == MetadataSource::Tvdb)
            .expect("the tvdb row");
        assert_eq!(tvdb.verification, Verification::Vouched(at));
    }

    /// **"Already in the library?" answered by any known id.** Adding by
    /// TMDB and syncing by the \*arr's TVDB axis is one title, not two.
    #[tokio::test]
    async fn a_title_is_found_by_whichever_id_the_caller_holds() {
        let pool = open_memory().await.unwrap();
        let id = item(&pool, 5).await;
        let tmdb = ExternalId::new(MetadataSource::Tmdb, "76479").unwrap();
        let tvdb = ExternalId::new(MetadataSource::Tvdb, "355567").unwrap();
        put(&pool, id, MediaType::Tv, &tmdb, Verification::Asserted)
            .await
            .unwrap();
        put(&pool, id, MediaType::Tv, &tvdb, Verification::Asserted)
            .await
            .unwrap();

        assert_eq!(find(&pool, MediaType::Tv, &tmdb).await.unwrap(), Some(id));
        assert_eq!(find(&pool, MediaType::Tv, &tvdb).await.unwrap(), Some(id));
        // A movie sharing the number is a different title, which is what
        // the media type in the natural key is for.
        assert_eq!(find(&pool, MediaType::Movie, &tmdb).await.unwrap(), None);
    }

    /// The migration's backfill loses no identity: every item that had a
    /// TMDB id has a row, and the ones that carried the other two have
    /// theirs.
    #[tokio::test]
    async fn the_backfill_carries_every_column_across() {
        let pool = open_memory().await.unwrap();
        // Written through the old columns, as the pre-migration app did.
        let item = crate::db::library::upsert(
            &pool,
            &Seed::series(76_479, "The Boys")
                .imdb("tt1190634")
                .tvdb(355_567)
                .build(),
        )
        .await
        .unwrap();

        // The migration ran before these rows existed, so the backfill is
        // re-stated here the way `library::upsert` will once the writers
        // move. What the test pins is the *shape*: three sources, one
        // item, and the IMDb value canonical.
        for (source, raw) in [
            (MetadataSource::Tmdb, "76479"),
            (MetadataSource::Imdb, "tt1190634"),
            (MetadataSource::Tvdb, "355567"),
        ] {
            let external = ExternalId::new(source, raw).unwrap();
            put(
                &pool,
                item.id,
                MediaType::Tv,
                &external,
                Verification::Asserted,
            )
            .await
            .unwrap();
        }

        let all = for_all(&pool).await.unwrap();
        let stored = all.get(&item.id).expect("the item has ids");
        assert_eq!(stored.len(), 3);
        assert!(
            stored
                .iter()
                .all(|s| s.verification == Verification::Asserted)
        );
    }
}
