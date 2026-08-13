//! `library_episode_numbering` — how a title is numbered *for search*,
//! when the canonical numbering is not the one releases use.
//!
//! See `migrations/20260808130000_episode_numbering.sql` for why this is
//! a translation table rather than a rewrite of `library_episodes`. The
//! one-line version: those two columns are also the row's identity, the
//! file name on disk and the pairing key with Sonarr, and only the
//! network coordinate needs to change.
//!
//! Everything here is keyed **canonical → group**, because that is the
//! direction every caller needs: the catalogue holds canonical numbers
//! and the question is always "what should I ask the indexer for".

use std::collections::HashMap;

use brarr_tmdb::EpisodeGroup;
use sqlx::Row;
use uuid::Uuid;

use crate::{AppError, db::Pool};

/// Where one episode sits under the alternate ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Numbering {
    /// Season number a release would use.
    pub season: i32,
    /// Episode number a release would use.
    pub episode: i32,
}

/// One row of the translation table, before it is persisted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumberingRow {
    /// The block's position in the group.
    pub part_order: i32,
    /// The block's name — an arc title, or a season name.
    pub part_name: Option<String>,
    /// What a release calls it.
    pub group: Numbering,
    /// What the catalogue calls it.
    pub canonical: Numbering,
    /// TMDB's own episode id, when the group carried one.
    pub tmdb_episode_id: Option<i64>,
}

/// Flatten a group into translation rows.
///
/// Pure, and separate from the persistence for the same reason
/// [`crate::tmdb_sync`]'s conversions are: the mapping is the part worth
/// testing, and it should be testable without a pool or a network.
///
/// **Blocks are 1-based and episodes are 0-based within their block** —
/// verified against a live capture of Jujutsu Kaisen's `季` group, whose
/// blocks run 1/2/3 with 24/23/12 episodes each and whose `order` restarts
/// at 0 inside every block. Reading `order` as global would put every
/// episode after the first block on the wrong season, which is the exact
/// failure this table exists to prevent, so both are guarded rather than
/// trusted: a non-positive block order falls back to its index.
#[must_use]
pub fn rows_from_group(group: &EpisodeGroup) -> Vec<NumberingRow> {
    let mut rows = Vec::new();
    for (index, part) in group.groups.iter().enumerate() {
        let part_order = if part.order > 0 {
            part.order
        } else {
            i32::try_from(index).unwrap_or(0) + 1
        };
        for (position, episode) in part.episodes.iter().enumerate() {
            let within = if episode.order >= 0 {
                episode.order + 1
            } else {
                i32::try_from(position).unwrap_or(0) + 1
            };
            rows.push(NumberingRow {
                part_order,
                part_name: part.name.clone(),
                group: Numbering {
                    season: part_order,
                    episode: within,
                },
                canonical: Numbering {
                    season: episode.season_number,
                    episode: episode.episode_number,
                },
                tmdb_episode_id: Some(episode.id),
            });
        }
    }
    rows
}

/// Persist a group as the title's search numbering.
///
/// Replaces whatever was there — the table is derived from TMDB and
/// holds no operator state, so a re-apply is a rebuild rather than a
/// merge. Writes **nothing** to `library_episodes` and touches no grab;
/// a test pins that.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn apply(
    pool: &Pool,
    item_id: Uuid,
    group_id: &str,
    group_name: Option<&str>,
    rows: &[NumberingRow],
) -> Result<u64, AppError> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM library_episode_numbering WHERE item_id = ?")
        .bind(item_id.to_string())
        .execute(&mut *tx)
        .await?;

    let mut written = 0_u64;
    for row in rows {
        // A group may list the same canonical episode twice (contributor
        // data), and the primary key would abort the whole apply. The
        // first placement wins and the rest are skipped: refusing the
        // ordering outright over one duplicate helps nobody.
        written += sqlx::query(
            "INSERT INTO library_episode_numbering ( \
                item_id, group_id, part_order, part_name, \
                group_season, group_episode, canonical_season, canonical_episode, \
                tmdb_episode_id \
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT DO NOTHING",
        )
        .bind(item_id.to_string())
        .bind(group_id)
        .bind(i64::from(row.part_order))
        .bind(row.part_name.as_deref())
        .bind(i64::from(row.group.season))
        .bind(i64::from(row.group.episode))
        .bind(i64::from(row.canonical.season))
        .bind(i64::from(row.canonical.episode))
        .bind(row.tmdb_episode_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    }

    sqlx::query("UPDATE library_items SET search_group_id = ?, search_group_name = ? WHERE id = ?")
        .bind(group_id)
        .bind(group_name)
        .bind(item_id.to_string())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(written)
}

/// Go back to the canonical numbering.
///
/// The whole undo, and it is one statement plus a delete — which is the
/// property that makes this safe to try on a live catalogue.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn clear(pool: &Pool, item_id: Uuid) -> Result<(), AppError> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM library_episode_numbering WHERE item_id = ?")
        .bind(item_id.to_string())
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE library_items SET search_group_id = NULL, search_group_name = NULL WHERE id = ?",
    )
    .bind(item_id.to_string())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Which ordering a title is currently searched under, if any.
///
/// A small query of its own rather than two more fields on
/// [`crate::db::library::LibraryItem`]: the screens that need this are
/// the ones already asking about groups, and the catalogue struct is
/// read on every render of a 363-title index.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn active_group(
    pool: &Pool,
    item_id: Uuid,
) -> Result<Option<(String, Option<String>)>, AppError> {
    let row =
        sqlx::query("SELECT search_group_id, search_group_name FROM library_items WHERE id = ?")
            .bind(item_id.to_string())
            .fetch_optional(pool)
            .await?;
    let Some(row) = row else { return Ok(None) };
    let id: Option<String> = row.try_get("search_group_id")?;
    let name: Option<String> = row.try_get("search_group_name")?;
    Ok(id.map(|id| (id, name)))
}

/// The translation for one title, canonical → group.
///
/// Empty when the title uses the canonical numbering, which is every
/// title until an operator says otherwise — so the caller's fallback is
/// "no entry means no translation", not a flag to check first.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn for_item(
    pool: &Pool,
    item_id: Uuid,
) -> Result<HashMap<(i32, i32), Numbering>, AppError> {
    let rows = sqlx::query(
        "SELECT canonical_season, canonical_episode, group_season, group_episode \
         FROM library_episode_numbering WHERE item_id = ?",
    )
    .bind(item_id.to_string())
    .fetch_all(pool)
    .await?;

    let mut map = HashMap::with_capacity(rows.len());
    for row in &rows {
        let cs: i64 = row.try_get("canonical_season")?;
        let ce: i64 = row.try_get("canonical_episode")?;
        let gs: i64 = row.try_get("group_season")?;
        let ge: i64 = row.try_get("group_episode")?;
        map.insert(
            (
                i32::try_from(cs).unwrap_or(0),
                i32::try_from(ce).unwrap_or(0),
            ),
            Numbering {
                season: i32::try_from(gs).unwrap_or(0),
                episode: i32::try_from(ge).unwrap_or(0),
            },
        );
    }
    Ok(map)
}

/// The translation for one title, **group → canonical**.
///
/// The mirror of [`for_item`], for the other question: the catalogue
/// asks "what should I request from the indexer", and the adoption path
/// asks "the file says S02E01 — which episode is that". Both directions
/// come out of the same rows, and the reverse is unambiguous because a
/// group places every episode exactly once (verified against the live
/// table: zero `(group_season, group_episode)` collisions).
///
/// Empty for every title with no ordering applied, so the caller's
/// fallback is a miss on an empty map rather than a flag to check.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn reverse_for_item(
    pool: &Pool,
    item_id: Uuid,
) -> Result<HashMap<(i32, i32), Numbering>, AppError> {
    let rows = sqlx::query(
        "SELECT canonical_season, canonical_episode, group_season, group_episode \
         FROM library_episode_numbering WHERE item_id = ?",
    )
    .bind(item_id.to_string())
    .fetch_all(pool)
    .await?;

    let mut map = HashMap::with_capacity(rows.len());
    for row in &rows {
        let cs: i64 = row.try_get("canonical_season")?;
        let ce: i64 = row.try_get("canonical_episode")?;
        let gs: i64 = row.try_get("group_season")?;
        let ge: i64 = row.try_get("group_episode")?;
        map.insert(
            (
                i32::try_from(gs).unwrap_or(0),
                i32::try_from(ge).unwrap_or(0),
            ),
            Numbering {
                season: i32::try_from(cs).unwrap_or(0),
                episode: i32::try_from(ce).unwrap_or(0),
            },
        );
    }
    Ok(map)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert on happy paths"
)]
mod tests {
    use super::*;
    use brarr_tmdb::{EpisodeGroupKind, EpisodeGroupPart, GroupEpisode};

    /// Jujutsu Kaisen's `季` group, reduced to the boundary that matters:
    /// the end of block 1 and the ends of blocks 2 and 3. Numbers are
    /// from the live capture in `brarr-tmdb`'s fixtures.
    fn jujutsu() -> EpisodeGroup {
        let block = |order: i32, name: &str, canon_start: i32, count: i32| EpisodeGroupPart {
            name: Some(name.to_owned()),
            order,
            episodes: (0..count)
                .map(|i| GroupEpisode {
                    id: i64::from(canon_start + i),
                    season_number: 1,
                    episode_number: canon_start + i,
                    order: i,
                    title: None,
                })
                .collect(),
        };
        EpisodeGroup {
            id: "6961c83d72e76980b8bd3780".to_owned(),
            name: Some("季".to_owned()),
            kind: EpisodeGroupKind::Production,
            groups: vec![
                block(1, "呪術廻戦", 1, 24),
                block(2, "懐玉・玉折／渋谷事変", 25, 23),
                block(3, "死滅回游", 48, 12),
            ],
        }
    }

    #[test]
    fn the_canonical_episode_the_scene_calls_s02e23_is_s01e47() {
        // The pairing this whole table exists for. `Jujutsu Kaisen S02E23`
        // is a real release name in this operator's database; TMDB calls
        // the same episode S01E47, which is what brarr was asking for.
        let rows = rows_from_group(&jujutsu());
        assert_eq!(rows.len(), 59);

        let hit = rows
            .iter()
            .find(|r| {
                r.canonical
                    == Numbering {
                        season: 1,
                        episode: 47,
                    }
            })
            .expect("canonical S01E47 is in the group");
        assert_eq!(
            hit.group,
            Numbering {
                season: 2,
                episode: 23
            }
        );
    }

    #[test]
    fn episode_order_restarts_inside_each_block() {
        // Reading `order` as global would put the first episode of block
        // 2 at S02E25 instead of S02E01 — every episode after block 1
        // lands on the wrong number, and the sweep asks for something
        // that does not exist.
        let rows = rows_from_group(&jujutsu());
        let first_of_block_2 = rows
            .iter()
            .find(|r| r.part_order == 2 && r.group.episode == 1)
            .expect("block 2 starts at episode 1");
        assert_eq!(
            first_of_block_2.canonical,
            Numbering {
                season: 1,
                episode: 25
            }
        );
    }

    #[tokio::test]
    async fn applying_and_clearing_round_trips() {
        use crate::db::library::{MediaType, NewLibraryItem, upsert};
        use crate::db::open_memory;

        let pool = open_memory().await.unwrap();
        let item = upsert(
            &pool,
            &NewLibraryItem {
                media_type: Some(MediaType::Tv),
                tmdb_id: 95479,
                title: "Jujutsu Kaisen".to_owned(),
                ..NewLibraryItem::default()
            },
        )
        .await
        .unwrap();

        assert!(for_item(&pool, item.id).await.unwrap().is_empty());

        let rows = rows_from_group(&jujutsu());
        let written = apply(
            &pool,
            item.id,
            "6961c83d72e76980b8bd3780",
            Some("季"),
            &rows,
        )
        .await
        .unwrap();
        assert_eq!(written, 59);

        let map = for_item(&pool, item.id).await.unwrap();
        assert_eq!(
            map.get(&(1, 47)),
            Some(&Numbering {
                season: 2,
                episode: 23
            })
        );

        clear(&pool, item.id).await.unwrap();
        assert!(
            for_item(&pool, item.id).await.unwrap().is_empty(),
            "reverting to the canonical numbering is the whole undo"
        );
    }

    /// **The contract.** Applying an ordering must not move one row of
    /// the catalogue and must not unlink one file. Renumbering
    /// `library_episodes` instead would delete ~117 rows for a title
    /// this size and null the same number of `grabs.episode_id`, with
    /// both repair paths inert — which is precisely why this table
    /// exists beside the tree rather than replacing it.
    /// One series shaped the way TMDB actually reports Jujutsu Kaisen:
    /// a single season of 59.
    async fn flat_series(pool: &Pool) -> crate::db::library::LibraryItem {
        use crate::db::library::{MediaType, NewEpisode, NewLibraryItem, NewSeason, upsert};

        let item = upsert(
            pool,
            &NewLibraryItem {
                media_type: Some(MediaType::Tv),
                tmdb_id: 95479,
                title: "Jujutsu Kaisen".to_owned(),
                ..NewLibraryItem::default()
            },
        )
        .await
        .unwrap();
        crate::db::library::sync_seasons(
            pool,
            item.id,
            &[NewSeason {
                season_number: 1,
                episode_count: 59,
                air_date: None,
                episodes: (1..=59)
                    .map(|n| NewEpisode {
                        tmdb_episode_id: None,
                        episode_number: n,
                        title: None,
                        air_date: None,
                    })
                    .collect(),
            }],
        )
        .await
        .unwrap();
        item
    }

    #[tokio::test]
    async fn applying_an_ordering_moves_no_episode_and_unlinks_no_file() {
        use crate::db::grabs::{self, NewGrab, Protocol};
        use crate::db::library;
        use crate::db::open_memory;

        let pool = open_memory().await.unwrap();
        let item = flat_series(&pool).await;

        let base_url = url::Url::parse("https://capybarabr.com/").unwrap();
        let provider = crate::db::providers::insert(
            &pool,
            crate::db::providers::NewProvider {
                name: "capybara",
                base_url: &base_url,
                api_token: "tok",
                kind: "unit3d",
                plugin_path: None,
            },
        )
        .await
        .unwrap();

        let episodes_before = library::episodes(&pool, item.id).await.unwrap();
        let held = episodes_before
            .iter()
            .find(|e| e.episode_number == 47)
            .unwrap();
        grabs::reserve(
            &pool,
            &NewGrab {
                item_id: item.id,
                episode_id: Some(held.id),
                season_number: None,
                decision_id: None,
                provider_id: provider.id,
                provider_name: "capybara",
                release_id_remote: "s01e47",
                release_name: "Jujutsu Kaisen S02E23",
                download_url: None,
                protocol: Protocol::Torrent,
            },
        )
        .await
        .unwrap()
        .unwrap();
        let grabs_before: Vec<_> = grabs::for_item(&pool, item.id)
            .await
            .unwrap()
            .into_iter()
            .map(|g| (g.id, g.episode_id))
            .collect();

        apply(
            &pool,
            item.id,
            "6961c83d72e76980b8bd3780",
            Some("季"),
            &rows_from_group(&jujutsu()),
        )
        .await
        .unwrap();

        let episodes_after = library::episodes(&pool, item.id).await.unwrap();
        assert_eq!(episodes_after.len(), episodes_before.len());
        for (before, after) in episodes_before.iter().zip(episodes_after.iter()) {
            assert_eq!(before.id, after.id, "the row identity must not move");
            assert_eq!(before.season_number, after.season_number);
            assert_eq!(before.episode_number, after.episode_number);
        }
        let grabs_after: Vec<_> = grabs::for_item(&pool, item.id)
            .await
            .unwrap()
            .into_iter()
            .map(|g| (g.id, g.episode_id))
            .collect();
        assert_eq!(
            grabs_before, grabs_after,
            "no acquisition may lose the episode it holds"
        );
    }

    #[tokio::test]
    async fn a_duplicate_canonical_episode_does_not_abort_the_apply() {
        use crate::db::library::{MediaType, NewLibraryItem, upsert};
        use crate::db::open_memory;

        let pool = open_memory().await.unwrap();
        let item = upsert(
            &pool,
            &NewLibraryItem {
                media_type: Some(MediaType::Tv),
                tmdb_id: 1,
                title: "T".to_owned(),
                ..NewLibraryItem::default()
            },
        )
        .await
        .unwrap();

        // Contributor data can place the same episode in two blocks.
        let mut rows = rows_from_group(&jujutsu());
        rows.push(rows[0].clone());

        let written = apply(&pool, item.id, "g", None, &rows).await.unwrap();
        assert_eq!(written, 59, "the duplicate is skipped, not fatal");
    }
}
