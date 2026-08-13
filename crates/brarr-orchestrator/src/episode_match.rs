//! Which catalogue episode a file is, when the file is not numbered the
//! way the catalogue is.
//!
//! [`crate::db::library`] holds TMDB's numbering, and TMDB is not the
//! only opinion about how a series is divided. Dragon Ball Super is one
//! season of 131 episodes there; `TheTVDB`, Sonarr, the operator's disk and
//! every release call it five seasons of 14/13/19/30/55. The files are
//! present and correctly named — just not in the coordinates the
//! catalogue uses — so `(season, episode)` alone finds fourteen of them
//! and the library reports 117 episodes missing that are on disk.
//!
//! ## Three tries, in this order, per file
//!
//! 1. **Canonical.** The coordinate as given. Every title where the two
//!    numberings agree resolves here and nothing below ever runs.
//! 2. **Absolute**, and only when the catalogue tree is a *single*
//!    season. That condition is not a heuristic: a one-season tree is
//!    precisely the shape TMDB produces when it flattens a series into
//!    airing order, and airing order is what an absolute number counts.
//!    Sonarr carries `absoluteEpisodeNumber` on **every** episode of this
//!    operator's anime catalogue, so this tier alone answers the fifteen
//!    affected series that have no episode group applied.
//! 3. **The applied episode group**, read backwards. Covers a title whose
//!    alternate ordering is not plain absolute — an arc split where the
//!    operator picked the ordering by hand.
//!
//! **Canonical first is the whole safety argument.** Os Simpsons has 801
//! files and thirty-seven seasons that both sides agree on; every one of
//! them resolves on the first try, so a fallback that would map absolute
//! 1..801 onto a season that does not exist never gets the chance to be
//! wrong. A tier only ever runs for a coordinate the tier above could not
//! place, and every tier still has to land on a row that exists.
//!
//! Measured against the operator's live catalogue before this existed:
//! 6 133 of 6 789 \*arr-paired files resolved. With the three tiers,
//! 6 765.
//!
//! Nothing here renumbers anything. The tree stays canonical — it is the
//! row identity, the name on disk and the pairing key with Sonarr, which
//! is the reasoning `migrations/20260808130000_episode_numbering.sql`
//! sets out at length.

use std::collections::{HashMap, HashSet};

use time::{Date, OffsetDateTime};
use uuid::Uuid;

use crate::db::episode_numbering::{Numbering, NumberingRow};
use crate::db::library::Episode;

/// Resolves a file's coordinates onto a catalogue episode.
///
/// Built once per title and reused across its files: a long-running
/// series is hundreds of lookups against the same two maps.
#[derive(Debug, Clone, Default)]
pub struct EpisodeMatcher {
    /// Canonical `(season, episode)` → episode row.
    tree: HashMap<(i32, i32), Uuid>,
    /// The only non-special season, when there is exactly one. `None`
    /// disables the absolute tier entirely.
    flat_season: Option<i32>,
    /// Group `(season, episode)` → canonical `(season, episode)`.
    reverse: HashMap<(i32, i32), Numbering>,
}

impl EpisodeMatcher {
    /// Build from a title's episode rows and its reverse numbering.
    ///
    /// `reverse` is empty for every title using TMDB's numbering, which
    /// is all of them until an operator applies an ordering — so the
    /// third tier costs a miss on an empty map, not a branch to check.
    #[must_use]
    pub fn new(episodes: &[Episode], reverse: HashMap<(i32, i32), Numbering>) -> Self {
        let tree: HashMap<(i32, i32), Uuid> = episodes
            .iter()
            .map(|e| ((e.season_number, e.episode_number), e.id))
            .collect();
        Self::from_tree(tree, reverse)
    }

    /// Build straight from a coordinate map, for callers that already
    /// hold one and for the tests below.
    #[must_use]
    pub fn from_tree(
        tree: HashMap<(i32, i32), Uuid>,
        reverse: HashMap<(i32, i32), Numbering>,
    ) -> Self {
        // Specials are excluded from the count deliberately: a series
        // whose only real season is 1 stays flat whether or not TMDB
        // also lists a season 0, and season 0 is not part of any
        // absolute numbering.
        let seasons: HashSet<i32> = tree.keys().map(|(s, _)| *s).filter(|s| *s > 0).collect();
        let flat_season = (seasons.len() == 1).then(|| seasons.into_iter().next().unwrap_or(1));
        Self {
            tree,
            flat_season,
            reverse,
        }
    }

    /// The episode a file at `(season, episode)` belongs to.
    ///
    /// `absolute` is the \*arr's `absoluteEpisodeNumber` when it has one;
    /// a caller reading coordinates off a file name passes `None`, since
    /// a name carrying an absolute number carries no `SxxEyy` to get
    /// here with in the first place.
    ///
    /// `None` means no tier placed it. That is a file brarr will not
    /// record, which is the right outcome — a wrong link looks correct
    /// and costs more than a gap.
    #[must_use]
    pub fn resolve(&self, season: i32, episode: i32, absolute: Option<i32>) -> Option<Uuid> {
        if let Some(id) = self.tree.get(&(season, episode)) {
            return Some(*id);
        }
        if let (Some(flat), Some(abs)) = (self.flat_season, absolute)
            && let Some(id) = self.tree.get(&(flat, abs))
        {
            return Some(*id);
        }
        let canonical = self.reverse.get(&(season, episode))?;
        self.tree
            .get(&(canonical.season, canonical.episode))
            .copied()
    }

    /// Whether an alternate ordering is in play for this title.
    #[must_use]
    pub fn has_ordering(&self) -> bool {
        !self.reverse.is_empty()
    }
}

/// One episode's coordinates in some external source's numbering.
///
/// Both sources brarr can derive a numbering from — Sonarr and TheTVDB —
/// hand over the same values, because Sonarr's numbering *is*
/// `TheTVDB`'s. One shape, one derivation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExternalNumber {
    /// Season the source puts it in.
    pub season: i32,
    /// Number within that season.
    pub episode: i32,
    /// Position in the series as a whole, when the source has one.
    pub absolute: Option<i32>,
    /// First air date. **The only value here that belongs to the
    /// episode rather than to a numbering scheme**, which is what makes
    /// it the arbiter when the two schemes disagree.
    pub aired: Option<OffsetDateTime>,
}

/// The calendar day an air date falls on, for joining two sources that
/// both know when an episode aired.
fn day(at: OffsetDateTime) -> Date {
    at.date()
}

/// Index of catalogue episodes by air day, keeping only the days that
/// name exactly one episode.
///
/// Uniqueness on both sides is the whole safety of this tier: a
/// double-length premiere, or a streaming batch drop, puts several
/// episodes on one day and the date stops identifying anything.
fn unique_by_day(catalogue: &[Episode]) -> HashMap<Date, (i32, i32)> {
    let mut seen: HashMap<Date, Vec<(i32, i32)>> = HashMap::new();
    for e in catalogue.iter().filter(|e| e.season_number > 0) {
        if let Some(at) = e.air_date {
            seen.entry(day(at))
                .or_default()
                .push((e.season_number, e.episode_number));
        }
    }
    seen.into_iter()
        .filter(|(_, v)| v.len() == 1)
        .filter_map(|(k, v)| v.first().map(|c| (k, *c)))
        .collect()
}

/// Build the canonical → external translation for one title.
///
/// **The matcher is built with an empty reverse map on purpose.**
/// Deriving a translation from a translation this function itself wrote
/// would be self-referential, and a wrong row would keep re-deriving
/// itself forever.
///
/// ## The air date decides, and the absolute number only breaks ties
///
/// The absolute axis was the primary join and it is **not** reliable, a
/// claim this module previously made in the opposite direction. Kaiju
/// No. 8 is the counterexample, measured live: `TheTVDB` gives absolute
/// 13 to a special it files under season 0, so its `S02E01` carries
/// absolute **14** while the same episode — same title, same air date,
/// 2025-07-19 — sits at position **13** of TMDB's flat season. Joining
/// on the absolute number shifted every episode of that season by one
/// and left one unmatched, silently.
///
/// The air date is the only value either side reports that belongs to
/// the *episode* rather than to a numbering scheme, so it arbitrates.
/// Audited across this operator's catalogue: of 15 titles it agreed with
/// the absolute axis on 14 and corrected the fifteenth, 11 rows.
///
/// It is not sufficient on its own — a batch drop or a double premiere
/// puts several episodes on one day — so it is used only where it names
/// exactly one episode on each side, and the absolute axis still answers
/// the rest.
///
/// Rows where both sides agree are **omitted**: absent *is* the encoding
/// of "no translation", so the Simpsons would otherwise carry 801 rows
/// saying what their absence says. An empty result therefore means "this
/// title is numbered the same by both", not "nothing was found".
#[must_use]
pub fn derive_numbering(catalogue: &[Episode], external: &[ExternalNumber]) -> Vec<NumberingRow> {
    let coordinates: HashMap<Uuid, (i32, i32)> = catalogue
        .iter()
        .map(|e| (e.id, (e.season_number, e.episode_number)))
        .collect();
    let matcher = EpisodeMatcher::new(catalogue, HashMap::new());
    let by_day = unique_by_day(catalogue);
    // The other half of the uniqueness rule: a day several external
    // episodes share names none of them.
    let mut external_days: HashMap<Date, usize> = HashMap::new();
    for n in external.iter().filter(|n| n.season > 0) {
        if let Some(at) = n.aired {
            *external_days.entry(day(at)).or_default() += 1;
        }
    }

    let mut rows = Vec::new();
    for n in external {
        let dated = n
            .aired
            .map(day)
            .filter(|d| external_days.get(d) == Some(&1))
            .and_then(|d| by_day.get(&d).copied());
        let canonical = if let Some(canonical) = dated {
            canonical
        } else {
            let Some(found) = matcher.resolve(n.season, n.episode, n.absolute) else {
                continue;
            };
            let Some(&canonical) = coordinates.get(&found) else {
                continue;
            };
            canonical
        };
        if canonical == (n.season, n.episode) {
            continue;
        }
        rows.push(NumberingRow {
            part_order: n.season,
            part_name: None,
            group: Numbering {
                season: n.season,
                episode: n.episode,
            },
            canonical: Numbering {
                season: canonical.0,
                episode: canonical.1,
            },
            tmdb_episode_id: None,
        });
    }
    rows
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod tests {
    use super::*;

    /// A canonical tree of `season × episodes`, numbered from 1.
    fn tree(seasons: &[(i32, i32)]) -> HashMap<(i32, i32), Uuid> {
        let mut map = HashMap::new();
        for &(season, count) in seasons {
            for number in 1..=count {
                map.insert((season, number), Uuid::new_v4());
            }
        }
        map
    }

    /// Dragon Ball Super's real shape: TMDB flattens 131 episodes into
    /// one season, the disk and Sonarr split them into five arcs.
    fn dbs_reverse() -> HashMap<(i32, i32), Numbering> {
        let arcs = [14, 13, 19, 30, 55];
        let mut map = HashMap::new();
        let mut canonical = 1;
        for (index, &size) in arcs.iter().enumerate() {
            for within in 1..=size {
                map.insert(
                    (i32::try_from(index).unwrap() + 1, within),
                    Numbering {
                        season: 1,
                        episode: canonical,
                    },
                );
                canonical += 1;
            }
        }
        map
    }

    /// The coordinate as given always wins, so a title where both
    /// numberings agree never reaches a fallback.
    #[test]
    fn canonical_is_tried_first() {
        let tree = tree(&[(1, 13), (2, 22)]);
        let wanted = *tree.get(&(2, 7)).unwrap();
        // An absolute number that would resolve elsewhere, and a reverse
        // map pointing somewhere else again: neither may be consulted.
        let mut reverse = HashMap::new();
        reverse.insert(
            (2, 7),
            Numbering {
                season: 1,
                episode: 1,
            },
        );
        let matcher = EpisodeMatcher::from_tree(tree, reverse);
        assert_eq!(matcher.resolve(2, 7, Some(20)), Some(wanted));
    }

    /// The tier that rescues the fifteen affected series with no episode
    /// group applied: Sonarr says S03E05, TMDB has one flat season, and
    /// the absolute number is the canonical episode number.
    #[test]
    fn a_flat_tree_resolves_by_absolute_number() {
        let tree = tree(&[(1, 131)]);
        let wanted = *tree.get(&(1, 47)).unwrap();
        let matcher = EpisodeMatcher::from_tree(tree, HashMap::new());
        assert_eq!(matcher.resolve(4, 1, Some(47)), Some(wanted));
    }

    /// **The guard that keeps Os Simpsons intact.** A tree with real
    /// seasons never consults the absolute axis, so absolute 47 cannot
    /// be read as season 1 episode 47 of a series that has both.
    #[test]
    fn a_seasoned_tree_never_falls_back_to_absolute() {
        let matcher = EpisodeMatcher::from_tree(tree(&[(1, 50), (2, 22)]), HashMap::new());
        assert_eq!(matcher.resolve(3, 1, Some(47)), None);
    }

    /// A season 0 alongside one real season still counts as flat —
    /// specials are not part of any absolute numbering.
    #[test]
    fn specials_do_not_make_a_tree_seasoned() {
        let mut map = tree(&[(1, 131)]);
        map.extend(tree(&[(0, 2)]));
        let wanted = *map.get(&(1, 47)).unwrap();
        let matcher = EpisodeMatcher::from_tree(map, HashMap::new());
        assert_eq!(matcher.resolve(4, 1, Some(47)), Some(wanted));
    }

    /// The applied ordering answers a coordinate the absolute tier
    /// cannot, because the file name carries no absolute number.
    #[test]
    fn an_applied_ordering_resolves_without_an_absolute_number() {
        let tree = tree(&[(1, 131)]);
        // Arc 4 episode 1 is canonical 47: 14 + 13 + 19 = 46 before it.
        let wanted = *tree.get(&(1, 47)).unwrap();
        let matcher = EpisodeMatcher::from_tree(tree, dbs_reverse());
        assert_eq!(matcher.resolve(4, 1, None), Some(wanted));
    }

    /// Every one of Dragon Ball Super's 131 files lands on its own
    /// episode, and no two land on the same one. This is the case the
    /// module exists for; before it, 14 resolved.
    #[test]
    fn every_dragon_ball_super_file_finds_a_distinct_episode() {
        let tree = tree(&[(1, 131)]);
        let matcher = EpisodeMatcher::from_tree(tree, dbs_reverse());
        let mut seen = HashSet::new();
        for (arc, size) in [14, 13, 19, 30, 55].iter().enumerate() {
            for within in 1..=*size {
                let arc = i32::try_from(arc).unwrap() + 1;
                let found = matcher
                    .resolve(arc, within, None)
                    .unwrap_or_else(|| panic!("S{arc:02}E{within:02} must resolve"));
                assert!(seen.insert(found), "S{arc:02}E{within:02} collided");
            }
        }
        assert_eq!(seen.len(), 131);
    }

    /// One catalogue episode, with an air date.
    fn ep(season: i32, number: i32, aired: Option<OffsetDateTime>) -> Episode {
        Episode {
            id: Uuid::new_v4(),
            tmdb_episode_id: None,
            item_id: Uuid::nil(),
            season_id: Uuid::nil(),
            season_number: season,
            episode_number: number,
            title: None,
            air_date: aired,
            monitored: true,
        }
    }

    /// **Kaiju No. 8, with the real numbers.**
    ///
    /// `TheTVDB` gives absolute 13 to a special it files under season 0,
    /// so its `S02E01` carries absolute **14** — while the same episode,
    /// same air date, is position **13** of TMDB's flat season. Joining
    /// on the absolute number shifts the whole season by one and orphans
    /// the thirteenth; joining on the date does not.
    ///
    /// Run against the absolute-first version this fails with the
    /// mapping one place out, which is exactly how it reached production.
    #[test]
    fn the_air_date_outranks_a_shifted_absolute_number() {
        use time::macros::datetime;

        let catalogue = vec![
            ep(1, 12, Some(datetime!(2024-06-29 0:00 UTC))),
            ep(1, 13, Some(datetime!(2025-07-19 0:00 UTC))),
            ep(1, 14, Some(datetime!(2025-07-26 0:00 UTC))),
        ];
        let external = vec![
            ExternalNumber {
                season: 1,
                episode: 12,
                absolute: Some(12),
                aired: Some(datetime!(2024-06-29 0:00 UTC)),
            },
            ExternalNumber {
                season: 2,
                episode: 1,
                absolute: Some(14),
                aired: Some(datetime!(2025-07-19 0:00 UTC)),
            },
            ExternalNumber {
                season: 2,
                episode: 2,
                absolute: Some(15),
                aired: Some(datetime!(2025-07-26 0:00 UTC)),
            },
        ];

        let rows = derive_numbering(&catalogue, &external);
        let of = |canonical: i32| {
            rows.iter()
                .find(|r| r.canonical.episode == canonical)
                .map(|r| r.group)
        };
        assert_eq!(
            of(13),
            Some(Numbering {
                season: 2,
                episode: 1
            }),
            "canonical 13 is S02E01 — the absolute number says 14 and is wrong"
        );
        assert_eq!(
            of(14),
            Some(Numbering {
                season: 2,
                episode: 2
            })
        );
        // Episode 12 agrees on both sides, so it contributes no row.
        assert_eq!(of(12), None);
    }

    /// A day several episodes share names none of them, so the absolute
    /// axis still answers — a batch drop must not scramble a season.
    #[test]
    fn a_shared_air_date_does_not_decide() {
        use time::macros::datetime;

        let same = Some(datetime!(2024-01-01 0:00 UTC));
        let catalogue = vec![ep(1, 1, same), ep(1, 2, same), ep(1, 3, same)];
        let external = vec![ExternalNumber {
            season: 2,
            episode: 1,
            absolute: Some(3),
            aired: same,
        }];
        let rows = derive_numbering(&catalogue, &external);
        assert_eq!(
            rows.first().map(|r| r.canonical.episode),
            Some(3),
            "the date named three episodes, so the absolute number decided"
        );
    }

    /// A coordinate no tier can place stays unplaced. Guessing is not on
    /// the table: a wrong link looks right.
    #[test]
    fn an_unplaceable_coordinate_is_refused() {
        let matcher = EpisodeMatcher::from_tree(tree(&[(1, 12)]), HashMap::new());
        assert_eq!(matcher.resolve(9, 9, Some(400)), None);
    }

    /// A reverse entry pointing at an episode the catalogue does not
    /// have resolves to nothing rather than to something nearby.
    #[test]
    fn an_ordering_pointing_outside_the_tree_resolves_to_nothing() {
        let mut reverse = HashMap::new();
        reverse.insert(
            (2, 1),
            Numbering {
                season: 1,
                episode: 999,
            },
        );
        let matcher = EpisodeMatcher::from_tree(tree(&[(1, 12)]), reverse);
        assert_eq!(matcher.resolve(2, 1, None), None);
    }
}
