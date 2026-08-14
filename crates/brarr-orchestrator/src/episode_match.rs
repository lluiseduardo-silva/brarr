//! Which catalogue episode a file is, when the file is not numbered the
//! way the catalogue is.
//!
//! ## What is left of this, and why
//!
//! This module used to carry three tiers and a derivation, because
//! [`crate::db::library`] held TMDB's numbering and TMDB is not the only
//! opinion about how a series is divided. Dragon Ball Super is one
//! season of 131 episodes there; TheTVDB, Sonarr, the operator's disk
//! and every release call it five seasons of 14/13/19/30/55, so
//! `(season, episode)` alone found fourteen files and the library
//! reported 117 episodes missing that were on disk.
//!
//! The tree is now built by whoever numbers the series the way releases
//! do, so the middle tier — a stored translation, read backwards — has
//! nothing to translate and is gone with the table behind it. The air
//! date arbitrating between two numberings did not disappear; it moved
//! to [`crate::structure::pair`], where it belongs, because it is how
//! two *trees* are joined rather than how a file is placed.
//!
//! ## Two tries, in this order, per file
//!
//! 1. **Canonical.** The coordinate as given. Every title whose tree
//!    already agrees with the file resolves here and nothing below runs.
//! 2. **Absolute**, and only when the tree is a *single* season — the
//!    shape a provider produces when it flattens a series into airing
//!    order.
//!
//! **Canonical first is the whole safety argument.** Os Simpsons has 801
//! files and thirty-seven seasons that both sides agree on; every one of
//! them resolves on the first try, so a fallback that would map absolute
//! 1..801 onto a season that does not exist never gets the chance to be
//! wrong. A tier only ever runs for a coordinate the tier above could
//! not place, and it still has to land on a row that exists.
//!
//! The absolute tier stays for one window that is real: a deployment
//! that catalogues before its TheTVDB credential is configured births
//! its series under TMDB, and those titles keep a flattened tree while
//! Sonarr pairs their files on TheTVDB coordinates. It is also why the
//! tier is deliberately narrow — the absolute number is evidence, never
//! a coordinate. TheTVDB gives absolute 13 to a Kaiju No. 8 special it
//! files under season 0, so its `S02E01` carries absolute 14; used as a
//! primary join that shifted a whole season by one, left one episode
//! unmatched, and put two files on one episode with nothing erroring.

use std::collections::{HashMap, HashSet};

use uuid::Uuid;

use crate::db::library::Episode;

/// Resolves a file's coordinates onto a catalogue episode.
///
/// Built once per title and reused across its files: a long-running
/// series is hundreds of lookups against the same map.
#[derive(Debug, Clone, Default)]
pub struct EpisodeMatcher {
    /// `(season, episode)` → episode row.
    tree: HashMap<(i32, i32), Uuid>,
    /// The only non-special season, when there is exactly one. `None`
    /// disables the absolute tier entirely.
    flat_season: Option<i32>,
}

impl EpisodeMatcher {
    /// Build from a title's episode rows.
    #[must_use]
    pub fn new(episodes: &[Episode]) -> Self {
        Self::from_tree(
            episodes
                .iter()
                .map(|e| ((e.season_number, e.episode_number), e.id))
                .collect(),
        )
    }

    /// Build straight from a coordinate map, for callers that already
    /// hold one and for the tests below.
    #[must_use]
    pub fn from_tree(tree: HashMap<(i32, i32), Uuid>) -> Self {
        // Specials are excluded from the count deliberately: a series
        // whose only real season is 1 stays flat whether or not a
        // provider also lists a season 0, and season 0 is not part of any
        // absolute numbering.
        let seasons: HashSet<i32> = tree.keys().map(|(s, _)| *s).filter(|s| *s > 0).collect();
        let flat_season = (seasons.len() == 1).then(|| seasons.into_iter().next().unwrap_or(1));
        Self { tree, flat_season }
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
    /// and costs more than a gap, and the caller counts it.
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
        None
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

    /// The coordinate as given always wins, so a title where both
    /// numberings agree never reaches a fallback.
    #[test]
    fn canonical_is_tried_first() {
        let tree = tree(&[(1, 13), (2, 22)]);
        let wanted = *tree.get(&(2, 7)).unwrap();
        let matcher = EpisodeMatcher::from_tree(tree);
        // An absolute number that would resolve elsewhere may not be
        // consulted.
        assert_eq!(matcher.resolve(2, 7, Some(1)), Some(wanted));
    }

    /// The shape a provider produces when it flattens a series into
    /// airing order: one season, and the absolute number *is* the
    /// episode number within it.
    #[test]
    fn a_flat_tree_resolves_by_absolute_number() {
        let tree = tree(&[(1, 131)]);
        let wanted = *tree.get(&(1, 47)).unwrap();
        let matcher = EpisodeMatcher::from_tree(tree);
        assert_eq!(matcher.resolve(4, 1, Some(47)), Some(wanted));
    }

    /// **The safety rule.** Os Simpsons has thirty-seven seasons both
    /// sides agree on; mapping absolute 1..801 onto a season that does
    /// not exist must never be attempted.
    #[test]
    fn a_seasoned_tree_never_falls_back_to_absolute() {
        let matcher = EpisodeMatcher::from_tree(tree(&[(1, 13), (2, 22)]));
        assert_eq!(matcher.resolve(3, 1, Some(20)), None);
    }

    /// A season 0 is a specials bucket, not a second season — a series
    /// with one real season stays flat beside it.
    #[test]
    fn specials_do_not_make_a_tree_seasoned() {
        let tree = tree(&[(0, 4), (1, 131)]);
        let wanted = *tree.get(&(1, 47)).unwrap();
        let matcher = EpisodeMatcher::from_tree(tree);
        assert_eq!(matcher.resolve(4, 1, Some(47)), Some(wanted));
    }

    /// A coordinate no tier places is refused rather than guessed at: a
    /// wrong link reads as a file brarr has, which is worse than a gap
    /// the caller counts.
    #[test]
    fn an_unplaceable_coordinate_is_refused() {
        let matcher = EpisodeMatcher::from_tree(tree(&[(1, 10)]));
        assert_eq!(matcher.resolve(1, 99, None), None);
        assert_eq!(matcher.resolve(1, 99, Some(400)), None);
    }
}
