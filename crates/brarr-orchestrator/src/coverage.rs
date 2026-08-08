//! What the library has, against what it is monitoring.
//!
//! Every number on the library screens comes from here, and they all
//! answer one question: **of the content the operator asked brarr to
//! chase, how much is on disk?**
//!
//! ## The denominator is what is monitored, never what exists
//!
//! A series with ten seasons where the operator monitors three is not
//! 30% complete — it is complete or it is not, over those three. Counting
//! against the whole catalogue would make every partially-monitored title
//! read as permanently behind, which is exactly the state most of this
//! operator's anime is in on purpose.
//!
//! **That includes specials.** Season 0 is excluded from the tree
//! *summary* — the line that says how big the series is — because 76
//! specials against 40 real episodes is not what anyone means by "the
//! show". But it is **not** excluded here, and the difference is the
//! whole point: monitoring is the operator's lever, and a rule that
//! ignores a season makes their click do nothing. The Familiar of Zero
//! has one monitored special, on disk, and read 49/49 instead of 50/50.
//!
//! The fear that motivated the old exclusion — a series showing 76
//! specials as a permanent gap — is measurable and did not happen: of 914
//! specials in this operator's catalogue exactly one is monitored, and it
//! has a file. Specials arrive unmonitored and stay that way unless
//! somebody says otherwise, which is precisely what should decide it.
//!
//! [`crate::scan`] agrees, as of v0.10.1. It used to skip season 0 when
//! building targets, which left the one asymmetry these screens had: a
//! monitored special with no file read as missing here and the sweep
//! refused to go after it. The exclusion was redundant anyway — the
//! `monitored` flag is what keeps the bucket out, and on this operator's
//! catalogue that is 1 special of 914. A lever that moves the count but
//! not the sweep is a broken lever.
//!
//! ## "Aired" splits missing from merely unreleased
//!
//! An episode that has not aired is not missing, and painting it red is
//! how a status colour stops meaning anything. The two get different
//! colours ([`ItemStatus::UpToDate`] against [`ItemStatus::Missing`]) and
//! the difference is the whole reason the status is not just a
//! percentage.
//!
//! An episode with **no** air date counts as unaired: TMDB leaves the
//! field blank until it schedules one. A movie with no digital release
//! date counts as **released** — the opposite default, because TMDB
//! simply has no digital date for most older films and treating that as
//! "not out yet" would paint half a film library purple.

use std::collections::HashMap;

use time::OffsetDateTime;
use uuid::Uuid;

use crate::db::grabs::{self, Coverage, Grab, GrabTarget};
use crate::db::library::{LibraryItem, MediaType, MonitoredEpisode};

/// How much of what is monitored is actually here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Progress {
    /// Monitored episodes, **specials included when they are
    /// monitored**. `1` for a monitored movie.
    pub total: usize,
    /// Of those, how many a live grab covers.
    pub have: usize,
    /// Monitored, already aired, and not covered — the number the
    /// operator can act on.
    pub aired_missing: usize,
    /// Monitored, not aired yet, and not covered. Not a gap.
    pub unaired_missing: usize,
}

impl Progress {
    /// `have / total` as a percentage, `0` when nothing is monitored.
    #[must_use]
    pub fn percent(self) -> u8 {
        if self.total == 0 {
            return 0;
        }
        let pct = self.have.saturating_mul(100) / self.total;
        u8::try_from(pct).unwrap_or(100).min(100)
    }
}

/// The colour on the card, and what it means.
///
/// Deliberately more than "complete or not": the two ways of being
/// incomplete — *nothing has aired* and *something aired and is missing*
/// — call for opposite reactions, and one colour for both would train the
/// operator to ignore it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemStatus {
    /// Not monitored. brarr makes no claim about it.
    Paused,
    /// Monitored, but nothing inside is — a series with every season off.
    Nothing,
    /// Monitored and nothing has aired yet.
    Upcoming,
    /// Something aired and is not here.
    Missing,
    /// Everything aired is here; more is still coming.
    UpToDate,
    /// Everything monitored is here, and there is no more.
    Complete,
}

impl ItemStatus {
    /// Read the status off a progress count.
    ///
    /// Order matters: `Missing` outranks everything, because one gap the
    /// operator can close is more useful on a card than the good news
    /// around it.
    #[must_use]
    pub fn of(monitored: bool, progress: Progress) -> Self {
        if !monitored {
            return Self::Paused;
        }
        if progress.total == 0 {
            return Self::Nothing;
        }
        if progress.aired_missing > 0 {
            return Self::Missing;
        }
        if progress.unaired_missing > 0 {
            // Nothing aired *and* nothing acquired means the title simply
            // has not started. With something in hand it is up to date.
            if progress.have == 0 {
                return Self::Upcoming;
            }
            return Self::UpToDate;
        }
        Self::Complete
    }

    /// Modifier for the CSS class — `lib-status-{tone}`.
    #[must_use]
    pub fn tone(self) -> &'static str {
        match self {
            Self::Paused => "paused",
            Self::Nothing => "nothing",
            Self::Upcoming => "upcoming",
            Self::Missing => "missing",
            Self::UpToDate => "current",
            Self::Complete => "complete",
        }
    }

    /// What the chip says.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Paused => "pausado",
            Self::Nothing => "nada monitorado",
            Self::Upcoming => "a estrear",
            Self::Missing => "faltando",
            Self::UpToDate => "em dia",
            Self::Complete => "completo",
        }
    }

    /// What the card should **call out**, as `(faltando, a estrear)`.
    ///
    /// Not the same as what is true. A paused title keeps its per-episode
    /// flags, so it can have four aired episodes and no files — and
    /// rendering "4 faltando" in red beside a grey "pausado" chip reads
    /// as a call to action that does not exist. brarr is not going to
    /// chase any of them; the honest card shows `0/4` and stops there.
    #[must_use]
    pub fn callout(self, progress: Progress) -> (usize, usize) {
        match self {
            Self::Missing => (progress.aired_missing, 0),
            Self::Upcoming | Self::UpToDate => (0, progress.unaired_missing),
            Self::Paused | Self::Nothing | Self::Complete => (0, 0),
        }
    }
}

/// Whether a date counts as already past. `None` is **not** past — the
/// caller decides whether that is the right default for its media type.
fn aired(date: Option<OffsetDateTime>, now: OffsetDateTime) -> bool {
    date.is_some_and(|d| d <= now)
}

/// Progress for every series, from two bulk reads.
///
/// Items with no monitored episode simply do not appear; the caller
/// treats a miss as [`Progress::default`].
#[must_use]
pub fn summarise(
    episodes: &[MonitoredEpisode],
    coverage: &[Coverage],
    now: OffsetDateTime,
) -> HashMap<Uuid, Progress> {
    // Group the coverage first: without it this is O(episodes × grabs),
    // which on this operator's collection is 4 500 × 3 200.
    let mut by_item: HashMap<Uuid, Vec<Coverage>> = HashMap::new();
    for row in coverage {
        by_item.entry(row.item_id).or_default().push(*row);
    }

    let mut out: HashMap<Uuid, Progress> = HashMap::new();
    for episode in episodes {
        let progress = out.entry(episode.item_id).or_default();
        progress.total += 1;
        let covered = by_item.get(&episode.item_id).is_some_and(|rows| {
            let target = GrabTarget::episode(episode.id, episode.season_number);
            rows.iter().any(|row| row.covers(target))
        });
        if covered {
            progress.have += 1;
        } else if aired(episode.air_date, now) {
            progress.aired_missing += 1;
        } else {
            progress.unaired_missing += 1;
        }
    }
    out
}

/// Progress for one movie.
///
/// A movie is one unit, and its "air date" is the digital release: buying
/// a search before it exists is what the theatrical-window warning on the
/// detail page already says. **A missing date reads as released**, unlike
/// an episode's — TMDB has no digital date for most older films.
#[must_use]
pub fn movie_progress(item: &LibraryItem, coverage: &[Coverage], now: OffsetDateTime) -> Progress {
    if !item.monitored {
        return Progress::default();
    }
    let covered = coverage
        .iter()
        .any(|row| row.item_id == item.id && row.covers(GrabTarget::item()));
    let released = item
        .digital_release_at
        .is_none_or(|date| aired(Some(date), now));
    Progress {
        total: 1,
        have: usize::from(covered),
        aired_missing: usize::from(!covered && released),
        unaired_missing: usize::from(!covered && !released),
    }
}

/// Progress for one catalogue entry, whichever kind it is.
#[must_use]
pub fn progress_of<S: std::hash::BuildHasher>(
    item: &LibraryItem,
    series: &HashMap<Uuid, Progress, S>,
    coverage: &[Coverage],
    now: OffsetDateTime,
) -> Progress {
    match item.media_type {
        MediaType::Movie => movie_progress(item, coverage, now),
        MediaType::Tv => series.get(&item.id).copied().unwrap_or_default(),
    }
}

/// What one episode row shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpisodeState {
    /// A file is recorded and still on disk.
    Downloaded,
    /// A grab is in flight.
    Downloading,
    /// Aired, and nothing covers it.
    Missing,
    /// Not aired yet. Not a gap.
    Unaired,
    /// brarr imported it and the file is no longer where it put it. Its
    /// own state rather than [`Self::Missing`]: the operator deleted
    /// something, or a mount is broken, and both are worth saying out
    /// loud instead of showing the same red as "never had it".
    Gone,
}

impl EpisodeState {
    /// Modifier for the CSS class — `ep-mark-{tone}`.
    #[must_use]
    pub fn tone(self) -> &'static str {
        match self {
            Self::Downloaded => "have",
            Self::Downloading => "busy",
            Self::Missing => "missing",
            Self::Unaired => "unaired",
            Self::Gone => "gone",
        }
    }

    /// Short label, used as the accessible name.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Downloaded => "baixado",
            Self::Downloading => "baixando",
            Self::Missing => "faltando",
            Self::Unaired => "não exibido",
            Self::Gone => "arquivo sumiu",
        }
    }
}

/// One episode's state plus the detail its tooltip carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpisodeMark {
    /// Which icon to draw.
    pub state: EpisodeState,
    /// What the tooltip says — the mapped file for a downloaded episode,
    /// the release name for one in flight. Empty when there is nothing to
    /// add beyond the label.
    ///
    /// The file name is the point: when a match is wrong, the only way to
    /// see *which* file brarr tied to this episode used to be reading the
    /// grab table row by row.
    pub detail: String,
    /// The grab this mark stands for, when one is in flight.
    ///
    /// Carried so the row can look its percentage up in the progress
    /// cache. `None` for every other state — a downloaded episode has
    /// nothing left to report, and a missing one has no grab at all.
    pub grab_id: Option<Uuid>,
}

/// Read one episode's state from the grabs of its item.
///
/// `grabs` is the item's whole history, including failed and vanished
/// rows — [`grabs::covers`] filters those out for the live question, and
/// the vanished ones are read separately, which is what makes
/// [`EpisodeState::Gone`] distinguishable from never having had it.
#[must_use]
pub fn episode_mark(
    episode_id: Uuid,
    season_number: i32,
    air_date: Option<OffsetDateTime>,
    grabs: &[Grab],
    now: OffsetDateTime,
) -> EpisodeMark {
    let target = GrabTarget::episode(episode_id, season_number);
    let live: Vec<&Grab> = grabs.iter().filter(|g| grabs::covers(g, target)).collect();

    if let Some(done) = live
        .iter()
        .find(|g| g.status == grabs::GrabStatus::Imported)
    {
        return EpisodeMark {
            state: EpisodeState::Downloaded,
            detail: done
                .imported_path
                .clone()
                .unwrap_or_else(|| done.release_name.clone()),
            grab_id: None,
        };
    }
    if let Some(busy) = live.first() {
        return EpisodeMark {
            state: EpisodeState::Downloading,
            detail: format!("{} — {}", busy.status.label(), busy.release_name),
            grab_id: Some(busy.id),
        };
    }

    // Nothing live. A row that named this episode and lost its file is a
    // different story from one that never existed.
    let vanished = grabs.iter().find(|g| {
        g.file_missing_at.is_some()
            && grabs::covers_target(g.scope, g.episode_id, g.season_number, target)
    });
    if let Some(gone) = vanished {
        return EpisodeMark {
            state: EpisodeState::Gone,
            detail: gone
                .imported_path
                .clone()
                .unwrap_or_else(|| gone.release_name.clone()),
            grab_id: None,
        };
    }

    EpisodeMark {
        state: if aired(air_date, now) {
            EpisodeState::Missing
        } else {
            EpisodeState::Unaired
        },
        detail: String::new(),
        grab_id: None,
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert on happy paths"
)]
mod tests {
    use super::*;
    use time::Duration;

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap()
    }

    fn ep(item: Uuid, season: i32, offset_days: i64) -> MonitoredEpisode {
        MonitoredEpisode {
            item_id: item,
            id: Uuid::new_v4(),
            season_number: season,
            air_date: Some(now() + Duration::days(offset_days)),
        }
    }

    fn cover_episode(item: Uuid, episode: Uuid) -> Coverage {
        Coverage {
            item_id: item,
            scope: grabs::GrabScope::Episode,
            episode_id: Some(episode),
            season_number: None,
        }
    }

    /// The operator's own example: three monitored seasons, 30 episodes,
    /// 28 in hand. The answer is 2 — not "28 of however many the series
    /// has".
    #[test]
    fn the_denominator_is_what_is_monitored() {
        let item = Uuid::new_v4();
        let episodes: Vec<MonitoredEpisode> = (0..30).map(|i| ep(item, 1 + i % 3, -30)).collect();
        let coverage: Vec<Coverage> = episodes
            .iter()
            .take(28)
            .map(|e| cover_episode(item, e.id))
            .collect();

        let out = summarise(&episodes, &coverage, now());
        let progress = out[&item];
        assert_eq!(progress.total, 30);
        assert_eq!(progress.have, 28);
        assert_eq!(progress.aired_missing, 2);
        assert_eq!(ItemStatus::of(true, progress), ItemStatus::Missing);
    }

    /// Reported from the screen: The Familiar of Zero has 1 + 13 + 12 +
    /// 12 + 12 = 50 monitored episodes — the first is a special the
    /// operator monitors and already has — and the card read 49/49.
    ///
    /// Excluding a whole season from the count also made the operator's
    /// own toggle do nothing: unmarking the specials changed no number,
    /// because they were never in one.
    #[test]
    fn a_monitored_special_counts() {
        let item = Uuid::new_v4();
        let mut episodes = vec![ep(item, 0, -3000)];
        for season in 1..=4 {
            for _ in 0..12 {
                episodes.push(ep(item, season, -3000));
            }
        }
        episodes.push(ep(item, 1, -3000)); // season 1 has 13
        let coverage: Vec<Coverage> = episodes.iter().map(|e| cover_episode(item, e.id)).collect();

        let progress = summarise(&episodes, &coverage, now())[&item];
        assert_eq!(progress.total, 50, "the monitored special is one of them");
        assert_eq!(progress.have, 50);
        assert_eq!(ItemStatus::of(true, progress), ItemStatus::Complete);
    }

    /// The other half of the same rule: an *unmonitored* special is not
    /// counted, which is what makes unmarking the season the operator's
    /// lever rather than a no-op.
    #[test]
    fn an_unmonitored_special_is_simply_absent() {
        let item = Uuid::new_v4();
        // `monitored_episodes` filters on the flag in SQL, so an
        // unmonitored row never reaches the summariser at all.
        let only_real: Vec<MonitoredEpisode> = (1..=3).map(|s| ep(item, s, -100)).collect();
        let progress = summarise(&only_real, &[], now())[&item];
        assert_eq!(progress.total, 3);
    }

    /// An unaired episode is not a gap, and must not turn the card red —
    /// otherwise every returning series is permanently "faltando" and the
    /// colour stops carrying information.
    #[test]
    fn an_unaired_episode_is_up_to_date_not_missing() {
        let item = Uuid::new_v4();
        let aired_ep = ep(item, 1, -10);
        let future = ep(item, 1, 10);
        let coverage = vec![cover_episode(item, aired_ep.id)];

        let progress = summarise(&[aired_ep, future], &coverage, now())[&item];
        assert_eq!(progress.have, 1);
        assert_eq!(progress.aired_missing, 0);
        assert_eq!(progress.unaired_missing, 1);
        assert_eq!(ItemStatus::of(true, progress), ItemStatus::UpToDate);
    }

    #[test]
    fn nothing_aired_yet_is_upcoming() {
        let item = Uuid::new_v4();
        let progress = summarise(&[ep(item, 1, 10), ep(item, 1, 20)], &[], now())[&item];
        assert_eq!(ItemStatus::of(true, progress), ItemStatus::Upcoming);
    }

    #[test]
    fn everything_in_hand_and_nothing_left_is_complete() {
        let item = Uuid::new_v4();
        let a = ep(item, 1, -20);
        let b = ep(item, 1, -10);
        let coverage = vec![cover_episode(item, a.id), cover_episode(item, b.id)];
        let progress = summarise(&[a, b], &coverage, now())[&item];
        assert_eq!(progress.percent(), 100);
        assert_eq!(ItemStatus::of(true, progress), ItemStatus::Complete);
    }

    /// A season pack answers for its own season and no other — the same
    /// rule `blocking_for` applies, reached through `Coverage::covers`.
    #[test]
    fn a_season_pack_covers_only_its_season() {
        let item = Uuid::new_v4();
        let s1 = ep(item, 1, -30);
        let s2 = ep(item, 2, -20);
        let pack = Coverage {
            item_id: item,
            scope: grabs::GrabScope::Season,
            episode_id: None,
            season_number: Some(1),
        };
        let progress = summarise(&[s1, s2], &[pack], now())[&item];
        assert_eq!(progress.have, 1);
        assert_eq!(progress.aired_missing, 1);
    }

    #[test]
    fn a_paused_item_makes_no_claim() {
        let progress = Progress {
            total: 10,
            have: 0,
            aired_missing: 10,
            unaired_missing: 0,
        };
        assert_eq!(ItemStatus::of(false, progress), ItemStatus::Paused);
    }

    /// A series catalogued with every season switched off — which is what
    /// the *arr import creates — must not read as "complete".
    #[test]
    fn a_series_with_nothing_monitored_is_not_complete() {
        let item = Uuid::new_v4();
        let out = summarise(&[], &[], now());
        assert!(!out.contains_key(&item));
        assert_eq!(
            ItemStatus::of(true, Progress::default()),
            ItemStatus::Nothing
        );
    }

    /// Found on screen: a paused series with four monitored episodes and
    /// no files rendered "4 faltando" in red next to a grey "pausado"
    /// chip. Both statements were true and together they said brarr was
    /// about to do something, which it was not.
    #[test]
    fn a_paused_title_calls_nothing_out() {
        let progress = Progress {
            total: 4,
            have: 0,
            aired_missing: 4,
            unaired_missing: 0,
        };
        assert_eq!(ItemStatus::Paused.callout(progress), (0, 0));
        assert_eq!(ItemStatus::Nothing.callout(progress), (0, 0));
        // Monitored, same numbers: now it is a call to action.
        assert_eq!(ItemStatus::Missing.callout(progress), (4, 0));
    }

    #[test]
    fn only_the_upcoming_states_call_out_what_has_not_aired() {
        let progress = Progress {
            total: 5,
            have: 3,
            aired_missing: 0,
            unaired_missing: 2,
        };
        assert_eq!(ItemStatus::UpToDate.callout(progress), (0, 2));
        assert_eq!(ItemStatus::Upcoming.callout(progress), (0, 2));
        assert_eq!(ItemStatus::Complete.callout(progress), (0, 0));
    }

    #[test]
    fn percent_is_bounded_and_safe_on_zero() {
        assert_eq!(Progress::default().percent(), 0);
        assert_eq!(
            Progress {
                total: 3,
                have: 1,
                aired_missing: 2,
                unaired_missing: 0
            }
            .percent(),
            33
        );
    }
}
