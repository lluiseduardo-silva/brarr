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

    sqlx::query(
        "UPDATE library_items \
         SET search_group_id = ?, search_group_name = ?, search_numbering_source = 'tmdb' \
         WHERE id = ?",
    )
    .bind(group_id)
    .bind(group_name)
    .bind(item_id.to_string())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(written)
}

/// Who decided how a title is numbered for search.
///
/// The question every writer has to answer before writing. See
/// `migrations/20260813130000_arr_numbering.sql`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Derived from the \*arr's own tree. The sweep keeps it current,
    /// and it is the **lowest**-ranking source: it only works while the
    /// \*arr brarr means to replace is still installed to be read.
    Arr,
    /// Derived from `TheTVDB` directly. Outranks [`Source::Arr`] because
    /// it is the same data at its origin — Sonarr is `TheTVDB`-numbered —
    /// without depending on Sonarr being there.
    Tvdb,
    /// The operator picked a TMDB episode group. The sweep leaves it be.
    Tmdb,
    /// The operator declared the block boundaries by hand. Also
    /// hands-off. Exists because not every title has an \*arr behind it
    /// and the \*arr is not always right either — Solo Leveling is one
    /// season of 25 on TMDB, two of 12 and 13 on TheTVDB, and releases
    /// follow the split. TMDB is not wrong; the people publishing cut
    /// somewhere else.
    Manual,
    /// The operator went back to the canonical numbering. Also
    /// hands-off — without this, "voltar ao original" would be undone by
    /// the next sweep half an hour later.
    Off,
}

impl Source {
    /// Whether a background sweep produced this, as opposed to the
    /// operator.
    #[must_use]
    pub fn is_automatic(self) -> bool {
        matches!(self, Self::Arr | Self::Tvdb)
    }

    /// Precedence. Higher wins, and a writer never overwrites something
    /// that ranks at or above it.
    ///
    /// The operator's three all sit at the top and are equal, because
    /// they are all the same statement — *I decided this* — and the last
    /// one made should stand. Below them `TheTVDB` beats the \*arr: same
    /// data at its origin, without needing Sonarr installed to read it.
    #[must_use]
    fn rank(self) -> u8 {
        match self {
            Self::Arr => 1,
            Self::Tvdb => 2,
            Self::Tmdb | Self::Manual | Self::Off => 3,
        }
    }

    /// Whether a sweep writing as `self` may replace what `current`
    /// left. `None` means nobody has decided, so anything may write.
    #[must_use]
    pub fn may_replace(self, current: Option<Self>) -> bool {
        current.is_none_or(|c| self.rank() >= c.rank() && !c.is_operator())
    }

    /// Whether the operator set this, rather than a sweep.
    #[must_use]
    pub fn is_operator(self) -> bool {
        matches!(self, Self::Tmdb | Self::Manual | Self::Off)
    }

    /// Column value.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Arr => "arr",
            Self::Tvdb => "tvdb",
            Self::Tmdb => "tmdb",
            Self::Manual => "manual",
            Self::Off => "off",
        }
    }

    /// What the panel calls it.
    #[must_use]
    pub fn description(self) -> &'static str {
        match self {
            Self::Arr => "derivada do Sonarr",
            Self::Tvdb => "derivada da TheTVDB",
            Self::Tmdb => "agrupamento do TMDB",
            Self::Manual => "blocos definidos por você",
            Self::Off => "numeração original do TMDB",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "arr" => Some(Self::Arr),
            "tvdb" => Some(Self::Tvdb),
            "tmdb" => Some(Self::Tmdb),
            "manual" => Some(Self::Manual),
            "off" => Some(Self::Off),
            _ => None,
        }
    }
}

/// Who set this title's numbering. `None` means nobody has, which is
/// what lets the \*arr sweep derive one.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn source(pool: &Pool, item_id: Uuid) -> Result<Option<Source>, AppError> {
    let row = sqlx::query("SELECT search_numbering_source FROM library_items WHERE id = ?")
        .bind(item_id.to_string())
        .fetch_optional(pool)
        .await?;
    let Some(row) = row else { return Ok(None) };
    let raw: Option<String> = row.try_get("search_numbering_source")?;
    Ok(raw.as_deref().and_then(Source::parse))
}

/// The name shown for a numbering the sweep derived.
pub const ARR_NUMBERING_NAME: &str = "numeração do Sonarr";

/// Synthetic group id for a numbering the sweep derived. Not a TMDB id,
/// and deliberately not shaped like one — the panel matches rows by id
/// and must not light one up.
pub const ARR_GROUP_ID: &str = "arr";

/// The name shown for a numbering derived from `TheTVDB`.
pub const TVDB_NUMBERING_NAME: &str = "numeração da TheTVDB";

/// Synthetic group id for a numbering derived from `TheTVDB`.
pub const TVDB_GROUP_ID: &str = "tvdb";

/// Store a numbering a background sweep derived.
///
/// **Refuses to overwrite a decision, or a better source.** A title the
/// operator settled ([`Source::is_operator`]) is left exactly as it is,
/// and so is one a higher-ranking sweep already wrote — the \*arr pass
/// must not walk back what `TheTVDB` established, since the \*arr is the
/// fallback for titles `TheTVDB` could not answer for. The return says
/// which happened.
///
/// Empty `rows` means the source numbers this title exactly the way TMDB
/// does, which is the common case (161 of this operator's 176 matched
/// series). It clears rather than writes: absent *is* the encoding of
/// "no translation", so 800 identity rows for the Simpsons would be
/// storage that says nothing.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn apply_derived(
    pool: &Pool,
    item_id: Uuid,
    from: Source,
    rows: &[NumberingRow],
) -> Result<bool, AppError> {
    let current = source(pool, item_id).await?;
    if !from.may_replace(current) {
        return Ok(false);
    }
    // Nothing to say about a title nobody has said anything about. The
    // sweep runs this for every series every cycle and most of them
    // number the same as TMDB; without this each would take a
    // transaction to rewrite nothing.
    if current.is_none() && rows.is_empty() {
        return Ok(true);
    }
    let (group_id, group_name) = match from {
        Source::Tvdb => (TVDB_GROUP_ID, TVDB_NUMBERING_NAME),
        _ => (ARR_GROUP_ID, ARR_NUMBERING_NAME),
    };

    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM library_episode_numbering WHERE item_id = ?")
        .bind(item_id.to_string())
        .execute(&mut *tx)
        .await?;

    for row in rows {
        sqlx::query(
            "INSERT INTO library_episode_numbering ( \
                item_id, group_id, part_order, part_name, \
                group_season, group_episode, canonical_season, canonical_episode, \
                tmdb_episode_id \
             ) VALUES (?, ?, ?, NULL, ?, ?, ?, ?, NULL) \
             ON CONFLICT DO NOTHING",
        )
        .bind(item_id.to_string())
        .bind(group_id)
        .bind(i64::from(row.part_order))
        .bind(i64::from(row.group.season))
        .bind(i64::from(row.group.episode))
        .bind(i64::from(row.canonical.season))
        .bind(i64::from(row.canonical.episode))
        .execute(&mut *tx)
        .await?;
    }

    // Nothing to translate reads as "no numbering applied", not as an
    // applied numbering that happens to be empty — otherwise the panel
    // would claim an ordering for every normal series in the catalogue.
    let (id, name, src) = if rows.is_empty() {
        (None, None, None)
    } else {
        (Some(group_id), Some(group_name), Some(from.label()))
    };
    sqlx::query(
        "UPDATE library_items \
         SET search_group_id = ?, search_group_name = ?, search_numbering_source = ? \
         WHERE id = ?",
    )
    .bind(id)
    .bind(name)
    .bind(src)
    .bind(item_id.to_string())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

/// One stretch of a canonical season that releases call a season of its
/// own.
///
/// Solo Leveling's shape: TMDB has one season of 25, releases have
/// `S01E01`–`S01E12` and `S02E01`–`S02E13`. Two blocks, cutting at 13.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Block {
    /// Canonical season the block is cut out of.
    pub canonical_season: i32,
    /// First canonical episode in the block, inclusive.
    pub first_episode: i32,
    /// Last canonical episode in the block, inclusive.
    pub last_episode: i32,
    /// Season number releases give this block.
    pub season: i32,
}

/// Turn declared blocks into translation rows.
///
/// Pure, and separate from the persistence for the same reason
/// [`rows_from_group`] is: the arithmetic is the part worth testing.
///
/// Episode numbering inside a block **restarts at 1**, which is the
/// whole point — `S01E13` becomes `S02E01`. A block whose bounds are
/// inverted contributes nothing rather than a reversed range.
#[must_use]
pub fn rows_from_blocks(blocks: &[Block]) -> Vec<NumberingRow> {
    let mut rows = Vec::new();
    for block in blocks {
        if block.last_episode < block.first_episode {
            continue;
        }
        for (offset, canonical) in (block.first_episode..=block.last_episode).enumerate() {
            let within = i32::try_from(offset).unwrap_or(0) + 1;
            rows.push(NumberingRow {
                part_order: block.season,
                part_name: None,
                group: Numbering {
                    season: block.season,
                    episode: within,
                },
                canonical: Numbering {
                    season: block.canonical_season,
                    episode: canonical,
                },
                tmdb_episode_id: None,
            });
        }
    }
    rows
}

/// Read block sizes the operator typed — `"12, 13"`, `"14 13 19 30 55"`.
///
/// Sizes rather than ranges because that is how the split is described
/// by the people who make it, and how the operator described it: two
/// blocks, of twelve and thirteen. Separators are commas, spaces or
/// semicolons, in any mixture, because insisting on one of them is a
/// form that rejects work for no reason.
///
/// # Errors
///
/// Returns a sentence for the operator when a token is not a positive
/// number.
pub fn parse_block_sizes(raw: &str) -> Result<Vec<i32>, String> {
    let mut sizes = Vec::new();
    for token in raw.split([',', ';', ' ', '\t', '\n']) {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        let value: i32 = token
            .parse()
            .map_err(|_| format!("\"{token}\" não é um número de episódios"))?;
        if value <= 0 {
            return Err(format!("um bloco não pode ter {value} episódios"));
        }
        sizes.push(value);
    }
    Ok(sizes)
}

/// Cut one canonical season into blocks of the given sizes.
///
/// The sizes must account for **every** episode in the season. A short
/// list would leave the tail on the canonical numbering while the head
/// was renumbered around it — half a season searched one way and half
/// the other, which is worse than either. Saying so is one sentence; the
/// silent version is a bug report three weeks later.
///
/// # Errors
///
/// Returns a sentence for the operator when the sizes do not add up.
pub fn blocks_for_season(
    canonical_season: i32,
    episodes: i32,
    sizes: &[i32],
    first_season: i32,
) -> Result<Vec<Block>, String> {
    if sizes.is_empty() {
        return Ok(Vec::new());
    }
    let total: i32 = sizes.iter().sum();
    if total != episodes {
        return Err(format!(
            "a temporada {canonical_season} tem {episodes} episódios e seus blocos somam {total}"
        ));
    }
    let mut blocks = Vec::with_capacity(sizes.len());
    let mut next = 1;
    for (index, &size) in sizes.iter().enumerate() {
        let season = first_season + i32::try_from(index).unwrap_or(0);
        blocks.push(Block {
            canonical_season,
            first_episode: next,
            last_episode: next + size - 1,
            season,
        });
        next += size;
    }
    Ok(blocks)
}

/// Synthetic group id for a numbering the operator declared by hand.
pub const MANUAL_GROUP_ID: &str = "manual";

/// The name shown for a numbering the operator declared by hand.
pub const MANUAL_NUMBERING_NAME: &str = "blocos definidos por você";

/// Persist hand-declared blocks as this title's search numbering.
///
/// Unlike [`apply_from_arr`] this **always** wins: it is the operator
/// speaking, and it is the escape hatch for the case where neither TMDB
/// nor the \*arr has the split the scene uses. Empty `blocks` is the
/// same as [`clear`].
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn apply_manual(pool: &Pool, item_id: Uuid, blocks: &[Block]) -> Result<u64, AppError> {
    let rows = rows_from_blocks(blocks);
    if rows.is_empty() {
        clear(pool, item_id).await?;
        return Ok(0);
    }
    let written = apply(
        pool,
        item_id,
        MANUAL_GROUP_ID,
        Some(MANUAL_NUMBERING_NAME),
        &rows,
    )
    .await?;
    sqlx::query("UPDATE library_items SET search_numbering_source = 'manual' WHERE id = ?")
        .bind(item_id.to_string())
        .execute(pool)
        .await?;
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
    // `'off'`, not NULL. NULL means "nobody decided" and the \*arr sweep
    // would re-derive a numbering within the half hour — a button that
    // undoes itself is a broken lever, which this repository has already
    // said once about season 0.
    sqlx::query(
        "UPDATE library_items \
         SET search_group_id = NULL, search_group_name = NULL, search_numbering_source = 'off' \
         WHERE id = ?",
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

    /// **The arbitration.** Four writers, one column, and the rule has to
    /// hold in both directions: a sweep must not walk back what the
    /// operator settled, and the \*arr fallback must not walk back what
    /// `TheTVDB` established — the \*arr exists for titles `TheTVDB`
    /// could not answer for, not to overrule it.
    #[test]
    fn a_sweep_never_overrules_a_better_source() {
        use Source::{Arr, Manual, Off, Tmdb, Tvdb};

        // Nobody has decided: anything may write.
        assert!(Arr.may_replace(None));
        assert!(Tvdb.may_replace(None));

        // TheTVDB outranks the *arr, both ways.
        assert!(Tvdb.may_replace(Some(Arr)));
        assert!(!Arr.may_replace(Some(Tvdb)));

        // A sweep refreshing its own work is the ordinary case.
        assert!(Arr.may_replace(Some(Arr)));
        assert!(Tvdb.may_replace(Some(Tvdb)));

        // The operator's three are untouchable by any sweep, including
        // `Off` — which is the whole reason `Off` is a value rather than
        // a NULL.
        for settled in [Tmdb, Manual, Off] {
            assert!(!Arr.may_replace(Some(settled)), "arr over {settled:?}");
            assert!(!Tvdb.may_replace(Some(settled)), "tvdb over {settled:?}");
            assert!(settled.is_operator());
        }
        assert!(!Arr.is_operator());
        assert!(!Tvdb.is_operator());
        assert!(Arr.is_automatic());
        assert!(Tvdb.is_automatic());
    }

    /// Every variant round-trips through the column, or a stored value
    /// reads back as "nobody decided" and a sweep overwrites a decision.
    #[test]
    fn every_source_round_trips_through_its_column_value() {
        for source in [
            Source::Arr,
            Source::Tvdb,
            Source::Tmdb,
            Source::Manual,
            Source::Off,
        ] {
            assert_eq!(Source::parse(source.label()), Some(source));
            assert!(!source.description().is_empty());
        }
        assert_eq!(Source::parse("nonsense"), None);
    }

    /// **Solo Leveling.** One season of 25 on TMDB, and every release is
    /// `S01E01`–`S01E12` then `S02E01`–`S02E13`. The operator says
    /// "12, 13" and the split falls where the publishers put it.
    #[test]
    fn hand_declared_blocks_cut_a_flat_season_where_releases_do() {
        let sizes = parse_block_sizes("12, 13").unwrap();
        let blocks = blocks_for_season(1, 25, &sizes, 1).unwrap();
        assert_eq!(
            blocks,
            vec![
                Block {
                    canonical_season: 1,
                    first_episode: 1,
                    last_episode: 12,
                    season: 1,
                },
                Block {
                    canonical_season: 1,
                    first_episode: 13,
                    last_episode: 25,
                    season: 2,
                },
            ]
        );

        let rows = rows_from_blocks(&blocks);
        assert_eq!(rows.len(), 25);
        // The whole point: canonical 13 is asked for as S02E01.
        let thirteenth = rows
            .iter()
            .find(|r| r.canonical.episode == 13)
            .expect("canonical 13 is covered");
        assert_eq!(
            thirteenth.group,
            Numbering {
                season: 2,
                episode: 1
            }
        );
        // And the first block is untouched, which is why it must still be
        // written: absent would mean canonical, and it *is* canonical
        // here — but the row costs nothing and keeps the block visible.
        let first = rows
            .iter()
            .find(|r| r.canonical.episode == 1)
            .expect("canonical 1 is covered");
        assert_eq!(
            first.group,
            Numbering {
                season: 1,
                episode: 1
            }
        );
    }

    /// Dragon Ball Super by hand, for an operator with no Sonarr behind
    /// the title: five arcs, and canonical 47 is the first of the fourth.
    #[test]
    fn blocks_can_express_an_arc_split() {
        let sizes = parse_block_sizes("14 13 19 30 55").unwrap();
        let rows = rows_from_blocks(&blocks_for_season(1, 131, &sizes, 1).unwrap());
        assert_eq!(rows.len(), 131);
        let forty_seventh = rows
            .iter()
            .find(|r| r.canonical.episode == 47)
            .expect("canonical 47 is covered");
        assert_eq!(
            forty_seventh.group,
            Numbering {
                season: 4,
                episode: 1
            }
        );
    }

    /// Sizes that do not account for every episode are refused with the
    /// arithmetic spelled out. Half a season searched one way and half
    /// the other is worse than either.
    #[test]
    fn blocks_that_do_not_add_up_are_refused() {
        let sizes = parse_block_sizes("12, 12").unwrap();
        let err = blocks_for_season(1, 25, &sizes, 1).unwrap_err();
        assert!(err.contains("25"), "{err}");
        assert!(err.contains("24"), "{err}");
    }

    /// A block list can start anywhere — a title whose first block the
    /// scene calls S02.
    #[test]
    fn blocks_need_not_start_at_season_one() {
        let blocks = blocks_for_season(1, 24, &[12, 12], 2).unwrap();
        assert_eq!(blocks[0].season, 2);
        assert_eq!(blocks[1].season, 3);
    }

    #[test]
    fn block_sizes_are_read_in_any_reasonable_spelling() {
        assert_eq!(parse_block_sizes("12,13").unwrap(), vec![12, 13]);
        assert_eq!(parse_block_sizes(" 12 ; 13 ").unwrap(), vec![12, 13]);
        assert_eq!(parse_block_sizes("14 13 19").unwrap(), vec![14, 13, 19]);
        assert!(parse_block_sizes("").unwrap().is_empty());
        assert!(parse_block_sizes("doze").is_err());
        assert!(parse_block_sizes("12, 0").is_err());
        assert!(parse_block_sizes("12, -3").is_err());
    }

    /// Nothing declared is not an error, it is "leave it alone".
    #[test]
    fn no_sizes_means_no_blocks() {
        assert!(blocks_for_season(1, 25, &[], 1).unwrap().is_empty());
        assert!(rows_from_blocks(&[]).is_empty());
    }

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
