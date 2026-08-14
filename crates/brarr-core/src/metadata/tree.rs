//! A season tree, in the coordinates it will be stored under.

use time::OffsetDateTime;

use super::MetadataSource;

/// A provider-neutral season/episode tree.
///
/// **The coordinates in here are the ones that get persisted.** Nothing
/// translates them afterwards; that is the whole design. The alternative
/// — storing one provider's numbering and translating at read time — was
/// tried, and it did not converge: the translation had to be applied at
/// the search axis, at the file pairing and at the import destination,
/// and each of those learned about it separately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesTree {
    /// Who produced this tree.
    ///
    /// Carried on the value rather than passed alongside it, because the
    /// writer has to refuse a tree whose source differs from the item's
    /// recorded owner. `library_seasons` has no origin column today, so
    /// a TMDB sweep and a TheTVDB sweep over the same item overwrite each
    /// other in silence.
    pub source: MetadataSource,
    /// The ordering these coordinates represent.
    pub ordering: Ordering,
    /// Seasons, in the order the provider listed them.
    pub seasons: Vec<TreeSeason>,
}

impl SeriesTree {
    /// Episodes across every season, specials included.
    #[must_use]
    pub fn episode_count(&self) -> usize {
        self.seasons.iter().map(|s| s.episodes.len()).sum()
    }

    /// Episodes per real season, in season order — the shape that says
    /// whether two providers agree. Dragon Ball Super is `[131]` on TMDB
    /// and `[14, 13, 19, 30, 55]` on TheTVDB.
    ///
    /// Season 0 is excluded: it is the specials bucket, it is not part of
    /// any ordering, and counting it would make every comparison
    /// disagree for a reason that has nothing to do with numbering.
    #[must_use]
    pub fn shape(&self) -> Vec<usize> {
        let mut real: Vec<&TreeSeason> = self.seasons.iter().filter(|s| s.number > 0).collect();
        real.sort_by_key(|s| s.number);
        real.iter().map(|s| s.episodes.len()).collect()
    }

    /// Re-cut this tree along operator-declared blocks.
    ///
    /// Lives here, not in either provider, for two reasons. It is pure
    /// arithmetic over a tree, so it is testable without a network; and
    /// it keeps [`MetadataProvider::tree`](super::MetadataProvider::tree)'s
    /// contract intact — a provider asked for [`Ordering::Manual`] fetches
    /// its default and calls this, so what comes back is still the
    /// coordinates that will be stored.
    ///
    /// Seasons named by no block are carried through **unchanged**, which
    /// is what lets an operator declare the one season that is cut wrong
    /// without restating the rest of the series. Specials are never cut.
    ///
    /// Renumbering runs from the first real season upward, so blocks of
    /// `[12, 13]` over one season of 25 produce seasons 1 and 2, and a
    /// second declared season lands after them.
    ///
    /// # Errors
    ///
    /// [`BlockError::DoesNotAddUp`] when the blocks declared for a season
    /// do not account for all of its episodes.
    pub fn recut(&self, blocks: &[Block]) -> Result<Self, BlockError> {
        let mut ordered: Vec<&TreeSeason> = self.seasons.iter().collect();
        ordered.sort_by_key(|s| s.number);

        let mut seasons = Vec::with_capacity(ordered.len());
        // Specials keep season 0 and take no part in the numbering.
        let mut next_number = 1;
        for season in ordered {
            if season.number <= 0 {
                seasons.push((*season).clone());
                continue;
            }
            let declared: Vec<u32> = blocks
                .iter()
                .filter(|b| b.season == season.number)
                .map(|b| b.size)
                .collect();
            if declared.is_empty() {
                seasons.push(TreeSeason {
                    number: next_number,
                    ..(*season).clone()
                });
                next_number += 1;
                continue;
            }
            let held = u32::try_from(season.episodes.len()).unwrap_or(u32::MAX);
            let total: u32 = declared.iter().sum();
            if total != held {
                return Err(BlockError::DoesNotAddUp {
                    season: season.number,
                    episodes: held,
                    declared: total,
                });
            }
            let mut taken = 0_usize;
            for size in declared {
                let size = size as usize;
                let slice = &season.episodes[taken..taken + size];
                seasons.push(TreeSeason {
                    number: next_number,
                    // The air date of a cut block is the first episode's,
                    // not the parent season's: a block that starts a year
                    // later is a different season to everyone but TMDB.
                    air_date: slice.first().and_then(|e| e.air_date),
                    episodes: slice
                        .iter()
                        .enumerate()
                        .map(|(index, episode)| TreeEpisode {
                            // Numbering inside a block restarts at 1 —
                            // that is the entire point. The external id
                            // does not move, so nothing loses its
                            // identity in the process.
                            number: i32::try_from(index).unwrap_or(0) + 1,
                            ..episode.clone()
                        })
                        .collect(),
                });
                next_number += 1;
                taken += size;
            }
        }

        Ok(Self {
            source: self.source,
            ordering: Ordering::Manual {
                blocks: blocks.to_vec(),
            },
            seasons,
        })
    }
}

/// One season.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeSeason {
    /// Season number. `0` is the specials bucket.
    pub number: i32,
    /// When the season started airing, when the provider says.
    pub air_date: Option<OffsetDateTime>,
    /// Episodes, in the order the provider listed them.
    pub episodes: Vec<TreeEpisode>,
}

/// One episode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEpisode {
    /// The provider's own episode id, as text.
    ///
    /// **Required, not `Option`.** It is the only identity that survives
    /// a renumbering. Without it, rows are deleted and reinserted, the
    /// foreign key nulls every acquisition hanging off them, and the
    /// damage renders as *complete* rather than as missing — a false
    /// negative of absence, invisible on screen. That incident is why
    /// this field has the type it has: a provider that cannot name its
    /// episodes cannot own a tree.
    pub external_id: String,
    /// Number within the season.
    pub number: i32,
    /// Title, in whatever language the provider was asked for.
    pub title: Option<String>,
    /// First air date.
    ///
    /// The only value here that belongs to the *episode* rather than to
    /// a numbering scheme, which is what makes it the arbiter when two
    /// providers disagree about where a season ends.
    pub air_date: Option<OffsetDateTime>,
    /// Position in the series as a whole, when the provider has one.
    ///
    /// **Advisory: never a join key.** TheTVDB gives absolute 13 to a
    /// Kaiju No. 8 special, so its `S02E01` carries absolute 14 and an
    /// absolute-first pairing shifts an entire season by one — silently,
    /// with two files landing on one episode and one of them dropped.
    pub absolute_number: Option<i32>,
}

/// brarr's own word for a kind of ordering.
///
/// The word is brarr's so that "is this series in absolute order?" is
/// answerable without comparing provider strings. [`Self::Other`] is the
/// escape that keeps the accompanying CHECK from becoming the same
/// defect one level down: an ordering no brarr word covers costs no
/// migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderingFamily {
    /// Whatever the provider returns when asked for nothing in
    /// particular.
    Default,
    /// Broadcast order — the split the scene follows.
    Aired,
    /// The DVD release's order.
    Dvd,
    /// One run of numbers straight through the series.
    Absolute,
    /// A named alternate ordering, typically story arcs.
    Alternate,
    /// The order episodes were produced in.
    Production,
    /// Blocks the operator declared.
    Manual,
    /// Something the words above do not cover.
    Other,
}

impl OrderingFamily {
    /// Whether this renumbers relative to the provider's own default.
    ///
    /// Offering the canonical ordering as a choice offers what the tree
    /// already is, which reads as a decision the operator does not have.
    #[must_use]
    pub const fn renumbers(self) -> bool {
        !matches!(self, Self::Default | Self::Aired)
    }

    /// Iteration order, defined by an exhaustive `match`.
    ///
    /// Same shape and same reason as [`crate::MetadataSource::all`]: an
    /// array literal never fails to compile for being short, so a new
    /// variant would ship uncovered with the suite green. Adding one here
    /// breaks `next` and the author has to say where it belongs — and, in
    /// this case, remember that the persisted counterpart is a CHECK
    /// constraint that has to grow with it.
    const fn next(self) -> Option<Self> {
        match self {
            Self::Default => Some(Self::Aired),
            Self::Aired => Some(Self::Dvd),
            Self::Dvd => Some(Self::Absolute),
            Self::Absolute => Some(Self::Alternate),
            Self::Alternate => Some(Self::Production),
            Self::Production => Some(Self::Manual),
            Self::Manual => Some(Self::Other),
            Self::Other => None,
        }
    }

    /// Every family, in a fixed order. What the guards walk.
    pub fn all() -> impl Iterator<Item = Self> {
        std::iter::successors(Some(Self::Default), |f| f.next())
    }

    /// The `library_items.structure_family` value.
    ///
    /// These strings are a CHECK constraint in the schema. Changing one
    /// is a migration, not an edit.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Aired => "aired",
            Self::Dvd => "dvd",
            Self::Absolute => "absolute",
            Self::Alternate => "alternate",
            Self::Production => "production",
            Self::Manual => "manual",
            Self::Other => "other",
        }
    }

    /// Inverse of [`Self::label`]. `None` for anything the CHECK would
    /// have refused.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        Self::all().find(|f| f.label() == raw)
    }
}

/// A full ordering choice.
///
/// [`Self::Manual`] carries its blocks in the type, so "the family says
/// manual but no blocks are stored" is unrepresentable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ordering {
    /// The provider's own default cut.
    Default,
    /// A named ordering the provider offers, identified by a handle only
    /// that provider interprets: a season-type segment for TheTVDB, an
    /// episode group's hex id for TMDB.
    Named {
        /// brarr's word for what kind of ordering this is.
        family: OrderingFamily,
        /// The provider's own identifier, opaque here.
        handle: Box<str>,
    },
    /// Blocks the operator declared, applied on top of the provider's
    /// default.
    Manual {
        /// Where the cuts fall.
        blocks: Vec<Block>,
    },
}

impl Ordering {
    /// brarr's word for this ordering.
    #[must_use]
    pub const fn family(&self) -> OrderingFamily {
        match self {
            Self::Default => OrderingFamily::Default,
            Self::Named { family, .. } => *family,
            Self::Manual { .. } => OrderingFamily::Manual,
        }
    }

    /// The provider's handle, where there is one.
    #[must_use]
    pub fn handle(&self) -> Option<&str> {
        match self {
            Self::Named { handle, .. } => Some(handle),
            Self::Default | Self::Manual { .. } => None,
        }
    }
}

/// One stretch of a provider season that releases call a season of its
/// own.
///
/// Sizes, not ranges, because that is how the split is described by the
/// people who make it: Solo Leveling is "two blocks, of twelve and
/// thirteen", not "1–12 and 13–25".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Block {
    /// The provider season being cut.
    pub season: i32,
    /// How many episodes fall in this block.
    pub size: u32,
}

impl Block {
    /// Read block sizes the operator typed — `"12, 13"`, `"14 13 19 30 55"`.
    ///
    /// Separators are commas, semicolons or whitespace in any mixture:
    /// insisting on one of them is a form that rejects work for no
    /// reason.
    ///
    /// # Errors
    ///
    /// [`BlockError`] for a token that is not a positive number. The
    /// caller words it — this crate does not know which screen is asking.
    pub fn parse_sizes(raw: &str) -> Result<Vec<u32>, BlockError> {
        let mut sizes = Vec::new();
        for token in raw.split([',', ';', ' ', '\t', '\n', '\r']) {
            let token = token.trim();
            if token.is_empty() {
                continue;
            }
            let value = token
                .parse::<u32>()
                .ok()
                .filter(|n| *n > 0)
                .ok_or_else(|| BlockError::NotACount {
                    token: token.to_owned(),
                })?;
            sizes.push(value);
        }
        Ok(sizes)
    }

    /// Cut one season of `episodes` into blocks of the given sizes.
    ///
    /// The sizes must account for **every** episode. A short list would
    /// leave the tail on the provider's numbering while the head was
    /// renumbered around it — half a season searched one way and half the
    /// other, which is worse than either. Saying so is one sentence; the
    /// silent version is a bug report three weeks later.
    ///
    /// # Errors
    ///
    /// [`BlockError::DoesNotAddUp`] when the sizes miss the total.
    pub fn cut(season: i32, episodes: u32, sizes: &[u32]) -> Result<Vec<Self>, BlockError> {
        if sizes.is_empty() {
            return Ok(Vec::new());
        }
        let declared: u32 = sizes.iter().copied().sum();
        if declared != episodes {
            return Err(BlockError::DoesNotAddUp {
                season,
                episodes,
                declared,
            });
        }
        Ok(sizes
            .iter()
            .map(|size| Self {
                season,
                size: *size,
            })
            .collect())
    }
}

/// Why declared blocks could not be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BlockError {
    /// A token that is not a positive episode count.
    #[error("`{token}` is not a positive episode count")]
    NotACount {
        /// What was typed.
        token: String,
    },
    /// The sizes do not cover the season.
    #[error("season {season} has {episodes} episodes and the blocks add up to {declared}")]
    DoesNotAddUp {
        /// The season being cut.
        season: i32,
        /// What it really holds.
        episodes: u32,
        /// What was declared.
        declared: u32,
    },
}

/// One ordering a provider offers for one series, for the picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructureVariant {
    /// brarr's word for it.
    pub family: OrderingFamily,
    /// The provider's own identifier.
    pub handle: String,
    /// What the provider calls it, for the screen.
    pub name: String,
    /// Episodes covered / episodes in the series, when the provider can
    /// say. A TMDB story-arc group can cover 48 of 59.
    pub coverage: Option<(u32, u32)>,
}

impl StructureVariant {
    /// The [`Ordering`] this variant selects.
    #[must_use]
    pub fn ordering(&self) -> Ordering {
        Ordering::Named {
            family: self.family,
            handle: self.handle.clone().into_boxed_str(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    fn ep(n: i32) -> TreeEpisode {
        TreeEpisode {
            external_id: format!("e{n}"),
            number: n,
            title: None,
            air_date: None,
            absolute_number: None,
        }
    }

    fn season(number: i32, count: i32) -> TreeSeason {
        TreeSeason {
            number,
            air_date: None,
            episodes: (1..=count).map(ep).collect(),
        }
    }

    fn tree(seasons: Vec<TreeSeason>) -> SeriesTree {
        SeriesTree {
            source: MetadataSource::Tvdb,
            ordering: Ordering::Default,
            seasons,
        }
    }

    /// **The comparison the whole refactor turns on**, and the reason
    /// season 0 cannot count: TheTVDB answers Dragon Ball Super with two
    /// specials alongside the five real seasons, and TMDB's flat season
    /// has none. Counting specials would make every pair disagree for a
    /// reason that is not numbering.
    #[test]
    fn shape_is_the_real_seasons_in_order() {
        let dbs = tree(vec![
            season(0, 2),
            season(3, 19),
            season(1, 14),
            season(2, 13),
            season(4, 30),
            season(5, 55),
        ]);
        assert_eq!(dbs.shape(), vec![14, 13, 19, 30, 55]);
        assert_eq!(
            dbs.episode_count(),
            133,
            "specials are held, just not counted"
        );

        let tmdb = tree(vec![season(1, 131)]);
        assert_ne!(dbs.shape(), tmdb.shape(), "this is the disagreement");
    }

    #[test]
    fn sizes_are_read_in_whatever_the_operator_types() {
        assert_eq!(Block::parse_sizes("12, 13").unwrap(), vec![12, 13]);
        assert_eq!(
            Block::parse_sizes("14 13 19 30 55").unwrap(),
            vec![14, 13, 19, 30, 55]
        );
        assert_eq!(Block::parse_sizes(" 12 ;13,  ").unwrap(), vec![12, 13]);
        assert_eq!(Block::parse_sizes("").unwrap(), Vec::<u32>::new());
    }

    #[test]
    fn a_block_of_no_episodes_is_refused() {
        assert!(Block::parse_sizes("12, 0").is_err());
        assert!(Block::parse_sizes("12, -1").is_err());
        assert!(Block::parse_sizes("doze").is_err());
    }

    /// Solo Leveling: one season of 25, cut where releases cut it.
    #[test]
    fn blocks_must_account_for_the_whole_season() {
        let blocks = Block::cut(1, 25, &[12, 13]).unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].size, 12);
        assert_eq!(blocks[1].size, 13);

        // Half the season renumbered and half not is worse than either.
        assert_eq!(
            Block::cut(1, 25, &[12]),
            Err(BlockError::DoesNotAddUp {
                season: 1,
                episodes: 25,
                declared: 12,
            })
        );
        assert!(Block::cut(1, 25, &[12, 14]).is_err());
    }

    /// "Manual, but no blocks" cannot be written down.
    #[test]
    fn a_manual_ordering_carries_its_blocks() {
        let ordering = Ordering::Manual {
            blocks: Block::cut(1, 25, &[12, 13]).unwrap(),
        };
        assert_eq!(ordering.family(), OrderingFamily::Manual);
        assert_eq!(ordering.handle(), None);
        let Ordering::Manual { blocks } = &ordering else {
            unreachable!()
        };
        assert_eq!(blocks.len(), 2);
    }

    /// **Solo Leveling, the case manual blocks exist for.** TMDB has one
    /// season of 25; releases have `S01E01`–`S01E12` and
    /// `S02E01`–`S02E13`. After the cut the tree *is* what releases say,
    /// so nothing has to translate it later.
    #[test]
    fn a_declared_cut_renumbers_the_tree_itself() {
        let flat = tree(vec![season(1, 25)]);
        let cut = flat.recut(&Block::cut(1, 25, &[12, 13]).unwrap()).unwrap();

        assert_eq!(cut.shape(), vec![12, 13]);
        assert_eq!(
            cut.ordering,
            Ordering::Manual {
                blocks: vec![
                    Block {
                        season: 1,
                        size: 12
                    },
                    Block {
                        season: 1,
                        size: 13
                    }
                ]
            }
        );
        // Numbering inside a block restarts at 1 — the whole point.
        assert_eq!(cut.seasons[1].episodes[0].number, 1);
        // And the identity does not move, so nothing comes unlinked.
        assert_eq!(cut.seasons[1].episodes[0].external_id, "e13");
        assert_eq!(cut.episode_count(), flat.episode_count());
    }

    /// A season nobody declared is carried through untouched, so the
    /// operator can fix the one season that is cut wrong without
    /// restating a forty-season series.
    #[test]
    fn an_undeclared_season_is_left_alone() {
        let original = tree(vec![season(0, 3), season(1, 25), season(2, 10)]);
        let cut = original
            .recut(&Block::cut(1, 25, &[12, 13]).unwrap())
            .unwrap();

        assert_eq!(cut.shape(), vec![12, 13, 10], "season 2 slid, not changed");
        let specials = cut.seasons.iter().find(|s| s.number == 0).unwrap();
        assert_eq!(specials.episodes.len(), 3, "specials are never cut");
        let slid = cut.seasons.iter().find(|s| s.number == 3).unwrap();
        assert_eq!(slid.episodes[0].external_id, "e1");
        assert_eq!(slid.episodes[0].number, 1, "its own numbering is intact");
    }

    /// A short list would leave the tail on the provider's numbering
    /// while the head was renumbered around it.
    #[test]
    fn a_cut_that_does_not_cover_the_season_is_refused() {
        let flat = tree(vec![season(1, 25)]);
        assert_eq!(
            flat.recut(&[Block {
                season: 1,
                size: 12
            }]),
            Err(BlockError::DoesNotAddUp {
                season: 1,
                episodes: 25,
                declared: 12,
            })
        );
    }

    /// Offering the canonical ordering as a choice offers what the tree
    /// already is.
    #[test]
    fn only_the_orderings_that_renumber_are_offered() {
        assert!(!OrderingFamily::Default.renumbers());
        assert!(!OrderingFamily::Aired.renumbers());
        for family in [
            OrderingFamily::Dvd,
            OrderingFamily::Absolute,
            OrderingFamily::Alternate,
            OrderingFamily::Production,
            OrderingFamily::Manual,
            OrderingFamily::Other,
        ] {
            assert!(family.renumbers(), "{family:?}");
        }
    }
}
