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

use uuid::Uuid;

use crate::db::episode_numbering::Numbering;
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
