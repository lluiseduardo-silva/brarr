//! The single door to the season tree.
//!
//! Every rebuild of a series' seasons and episodes goes through
//! [`apply`], and [`crate::db::library::sync_seasons`] — the only code in
//! brarr that issues `INSERT INTO library_seasons` / `library_episodes`
//! — is reachable from nowhere else outside `db`. That is not tidiness.
//! `arr_import::sync_tree` calls the tree writer for **every** series on
//! **every** passive sweep, outside the `if created` gate, every half
//! hour; anything that rewrites a tree without first asking who owns it
//! has a half-life of one cycle.
//!
//! ## What this module refuses, and why each refusal exists
//!
//! A tree write re-points every acquisition hanging off the item, and the
//! damage is invisible: `grabs.episode_id` is `ON DELETE SET NULL`, so a
//! pruned episode silently unlinks its file, and before `grabs.scope`
//! existed such a row read as "covers the whole item" and rendered the
//! series *complete*. A false negative of absence shows up as a red card;
//! this shows up as nothing at all. So the gates are:
//!
//! 1. **A tree from the wrong source.** `library_items.structure_source`
//!    records who owns the shape. A TMDB sweep and a TheTVDB sweep over
//!    the same item would otherwise overwrite each other in silence,
//!    because `library_seasons` has no origin column and never will.
//!    `NULL` means *unclaimed* — every series catalogued since the
//!    identity migration has it blank — so it is adopted, not refused.
//! 2. **An empty tree.** Never `Ok(())` over a live tree: it would prune
//!    every episode and orphan every grab on the item. This mirrors
//!    [`brarr_core::MetadataError::Empty`], which exists so that "the
//!    provider answered with nothing" is not spelled the same way as
//!    "the two agree".
//! 3. **Orphans.** A stored episode the incoming tree does not cover, and
//!    which carries acquisitions, is a file about to be unlinked.
//! 4. **A move with no evidence.** [`MIN_AIR_DATE_COVERAGE`] — the gate
//!    the orphan check cannot stand in for, because a *uniform shift*
//!    consumes every row on both sides: each stored episode pairs with
//!    its neighbour, `orphans` comes back empty, and every episode ends
//!    up covered by the file of the one next to it. Green screen, wrong
//!    library.
//!
//! And the net under all four, measured inside the transaction: the count
//! of `grabs` for this item with `scope = 'episode'` and no `episode_id`.
//! One query, before and after. It fires even when the pairing went wrong
//! in a way none of the gates predicted, which is the point — the damage
//! it guards has no other symptom.
//!
//! ## What this module deliberately does not do
//!
//! It does not consult `settings::is_paused`. The pause exists to freeze
//! acquisition, and this engine has to be exercised by the ordinary
//! TMDB→TMDB refresh **while** production is paused — that window is the
//! only honest way to trust it before anything changes owner.

use std::collections::{HashMap, HashSet};

use brarr_core::{Block, MetadataSource, Ordering, OrderingFamily, SeriesTree, TreeEpisode};
use time::Date;
use uuid::Uuid;

use crate::db::library::{self, Episode};
use crate::db::{Pool, grabs};
use crate::error::AppError;

/// Share of episodes that must carry an air date on **both** sides before
/// a pairing that moves a stored episode may commit.
///
/// Below it there is no evidence to tell a genuine renumbering from a
/// uniform shift, and the two are indistinguishable by every other gate:
/// TheTVDB gives absolute 13 to a Kaiju No. 8 special, so its `S02E01`
/// carries absolute 14 and an absolute-first join moves an entire season
/// by one — consuming every row on both sides and leaving `orphans`
/// empty.
pub const MIN_AIR_DATE_COVERAGE: f32 = 0.5;

/// How one stored episode came to be matched to an incoming one.
///
/// A closed set of **methods**, not of providers. On the day a bad tree
/// write shows up, "which tier linked this" is the first question and the
/// only one that finds the rest of the batch.
///
/// The order of the variants is the order the tiers run in, and that
/// order is the whole safety argument — it has been inverted once, and it
/// shipped a season shifted by one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LinkMethod {
    /// The owning source names this row: same `source`, same
    /// `external_id`. A refresh, not a switch.
    Owner,
    /// The row predates the identity migration and carries only
    /// `tmdb_episode_id`. Reached solely for a TMDB-owned tree, because
    /// for any other owner that column is a foreign namespace and a
    /// numeric collision between two providers' episode ids means
    /// nothing.
    ExternalId,
    /// Same `(season, episode)` as before. Not identity — it is exactly
    /// the key a renumbering changes — but it is what keeps a tree whose
    /// rows have no stored id from being deleted and reinserted.
    Coordinates,
    /// A calendar day that names exactly one episode on each side.
    AirDate,
    /// Position in the series as a whole. **Advisory and last.** It is
    /// only reachable when the stored tree is a single season, which is
    /// the shape a flattened series has and the only shape in which a
    /// stored episode number *is* an absolute number.
    Absolute,
}

/// One stored episode matched to a coordinate in the incoming tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pairing {
    /// The stored row's id — the UUID the writer must reuse.
    pub stored_id: Uuid,
    /// Season the incoming tree puts it in.
    pub season: i32,
    /// Episode number the incoming tree gives it.
    pub number: i32,
    /// Which tier matched.
    pub method: LinkMethod,
    /// Whether the incoming coordinates differ from the stored ones.
    ///
    /// This is what arms [`MIN_AIR_DATE_COVERAGE`]: a refresh in which
    /// nothing moves needs no air dates to justify itself.
    pub moved: bool,
}

/// A stored episode the incoming tree does not cover.
#[derive(Debug, Clone)]
pub struct Orphan {
    /// The row that would be deleted.
    pub episode_id: Uuid,
    /// Where it sits today.
    pub season: i32,
    /// Its number today.
    pub number: i32,
    /// Title, so a report names something a human recognises.
    pub title: Option<String>,
    /// Acquisitions that would lose their episode. Non-zero is what makes
    /// an orphan damage rather than housekeeping.
    pub grabs: i64,
}

/// A season pack whose meaning changes because its season did.
///
/// `covers_target` answers per `season_number`, so Dragon Ball Super's
/// season-1 pack narrows from 131 episodes to 14 the instant a tree
/// switches. That is the correction, not a regression — but it has to be
/// said, not discovered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackImpact {
    /// The season the pack was recorded against.
    pub season: i32,
    /// Episodes that season holds today.
    pub was: usize,
    /// Episodes it would hold after the write.
    pub now: usize,
    /// Packs recorded against it.
    pub grabs: i64,
}

/// What a tree write would do, computed before anything is written.
///
/// Same discipline as the \*arr import preview: the number that decides
/// ("how many files would end up with no episode") has to be readable
/// beforehand, not discovered from a failed grab hours later.
#[derive(Debug, Clone)]
pub struct StructurePlan {
    /// Series this is about.
    pub item_id: Uuid,
    /// Who produced the incoming tree.
    pub source: MetadataSource,
    /// Every stored episode that survives, and where it lands.
    pub pairs: Vec<Pairing>,
    /// Stored episodes nothing claimed. **Must be empty to commit.**
    pub orphans: Vec<Orphan>,
    /// Incoming episodes with no stored counterpart.
    pub added: usize,
    /// Share of episodes carrying an air date, `(stored, incoming)`.
    pub air_date_coverage: (f32, f32),
    /// Season packs whose meaning changes.
    pub packs_affected: Vec<PackImpact>,
}

impl StructurePlan {
    /// Whether any stored episode changes coordinates.
    #[must_use]
    pub fn moves_anything(&self) -> bool {
        self.pairs.iter().any(|p| p.moved)
    }

    /// Whether the air dates are thick enough to justify a move.
    #[must_use]
    pub fn air_dates_are_thin(&self) -> bool {
        let (stored, incoming) = self.air_date_coverage;
        stored < MIN_AIR_DATE_COVERAGE || incoming < MIN_AIR_DATE_COVERAGE
    }

    /// Acquisitions that would be unlinked if this were committed anyway.
    #[must_use]
    pub fn grabs_at_risk(&self) -> i64 {
        self.orphans.iter().map(|o| o.grabs).sum()
    }
}

/// The cut an operator declared, in the form they declared it.
///
/// Stored as JSON in `library_items.structure_recipe`, and stored at all
/// because [`Ordering::Manual`] applied once and then forgotten is a
/// choice with a half-life of one refresh: `SeriesTree::recut` needs the
/// sizes again every time the provider's tree is fetched, and a tree that
/// grew an episode has to fail loudly rather than quietly revert.
///
/// Sizes per season, not a flat block list, because that is the shape the
/// form edits — one field per season — and a value that has to be
/// regrouped before it can be shown back is a value that will be
/// regrouped wrongly one day. The persistence shape lives here rather
/// than on [`brarr_core::Block`] so the vocabulary crate stays free of a
/// storage format.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Recipe {
    /// One entry per provider season the operator cut, in order.
    pub seasons: Vec<RecipeSeason>,
}

/// One season's declared cut.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RecipeSeason {
    /// The provider season being cut.
    pub season: i32,
    /// How many episodes fall in each block, in order.
    pub sizes: Vec<u32>,
}

impl Recipe {
    /// The blocks this recipe declares.
    #[must_use]
    pub fn blocks(&self) -> Vec<Block> {
        self.seasons
            .iter()
            .flat_map(|s| {
                s.sizes.iter().map(|size| Block {
                    season: s.season,
                    size: *size,
                })
            })
            .collect()
    }

    /// Read a recipe back, or nothing at all.
    ///
    /// A recipe that does not parse reads as absent rather than as an
    /// error: the column is only ever written by this module, so a value
    /// that fails is a value from a future brarr, and the item falling
    /// back to the provider's own ordering is a visible wrong shape the
    /// operator can re-declare — better than a title that refuses to
    /// refresh at all.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        serde_json::from_str(raw).ok()
    }

    /// Render for storage.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Json`] if the recipe cannot be serialised,
    /// which is unreachable for a tree of integers.
    pub fn render(&self) -> Result<String, AppError> {
        Ok(serde_json::to_string(self)?)
    }
}

/// The outcome of pairing one stored tree against one incoming tree.
#[derive(Debug, Clone, Default)]
pub struct Paired {
    /// Keyed by the coordinates the incoming tree uses, which is how the
    /// writer looks a row's UUID up.
    pub by_coordinate: HashMap<(i32, i32), Pairing>,
    /// Stored rows nothing claimed, by id.
    pub unclaimed: Vec<Uuid>,
}

/// Match every incoming episode to a stored row, best evidence first.
///
/// **A stored row is claimed at most once.** Without that rule the same
/// row can answer for two incoming episodes — one matching by id, one by
/// coordinates — and the second write overwrites the first, leaving the
/// tree silently one episode short with nothing pruned and nothing
/// logged. `StoredTree::resolve` has that hole today; it is unreachable
/// while ids are stable, and it is exactly what a change of owner would
/// expose.
///
/// The tiers run one at a time over the whole payload, rather than
/// best-tier-per-episode, so a weak match can never take a row that a
/// stronger match for another episode still needs.
#[must_use]
pub fn pair(stored: &[Episode], incoming: &SeriesTree) -> Paired {
    let flat: Vec<(i32, &TreeEpisode)> = incoming
        .seasons
        .iter()
        .flat_map(|s| s.episodes.iter().map(move |e| (s.number, e)))
        .collect();

    let mut claimed: HashSet<Uuid> = HashSet::new();
    let mut out: HashMap<(i32, i32), Pairing> = HashMap::with_capacity(flat.len());

    for tier in [
        LinkMethod::Owner,
        LinkMethod::ExternalId,
        LinkMethod::Coordinates,
        LinkMethod::AirDate,
        LinkMethod::Absolute,
    ] {
        let index = build_index(tier, stored, &flat, incoming.source);
        for (season, episode) in &flat {
            let coord = (*season, episode.number);
            if out.contains_key(&coord) {
                continue;
            }
            let Some(row) = index.lookup(episode, coord) else {
                continue;
            };
            if claimed.contains(&row.id) {
                continue;
            }
            claimed.insert(row.id);
            out.insert(
                coord,
                Pairing {
                    stored_id: row.id,
                    season: *season,
                    number: episode.number,
                    method: tier,
                    moved: (row.season_number, row.episode_number) != coord,
                },
            );
        }
    }

    let unclaimed = stored
        .iter()
        .filter(|e| !claimed.contains(&e.id))
        .map(|e| e.id)
        .collect();

    Paired {
        by_coordinate: out,
        unclaimed,
    }
}

/// One tier's view of the stored rows.
///
/// Built per tier rather than once, because each tier keys on something
/// different and two of them are only legal under a condition the whole
/// tree has to satisfy.
struct Index<'a> {
    by_identity: HashMap<String, &'a Episode>,
    by_coordinate: HashMap<(i32, i32), &'a Episode>,
    by_day: HashMap<Date, &'a Episode>,
    by_absolute: HashMap<i32, &'a Episode>,
    tier: LinkMethod,
}

impl<'a> Index<'a> {
    fn lookup(&self, episode: &TreeEpisode, coord: (i32, i32)) -> Option<&'a Episode> {
        match self.tier {
            LinkMethod::Owner | LinkMethod::ExternalId => {
                self.by_identity.get(&episode.external_id).copied()
            }
            LinkMethod::Coordinates => self.by_coordinate.get(&coord).copied(),
            LinkMethod::AirDate => episode
                .air_date
                .and_then(|d| self.by_day.get(&d.date()))
                .copied(),
            LinkMethod::Absolute => episode
                .absolute_number
                .and_then(|n| self.by_absolute.get(&n))
                .copied(),
        }
    }
}

fn build_index<'a>(
    tier: LinkMethod,
    stored: &'a [Episode],
    incoming: &[(i32, &TreeEpisode)],
    source: MetadataSource,
) -> Index<'a> {
    let mut index = Index {
        by_identity: HashMap::new(),
        by_coordinate: HashMap::new(),
        by_day: HashMap::new(),
        by_absolute: HashMap::new(),
        tier,
    };

    match tier {
        LinkMethod::Owner => {
            for row in stored {
                if row.source == Some(source) {
                    if let Some(id) = row.external_id.as_ref() {
                        index.by_identity.insert(id.clone(), row);
                    }
                }
            }
        }
        LinkMethod::ExternalId => {
            // Only a TMDB-owned tree may read `tmdb_episode_id`. For any
            // other owner it is a foreign namespace, and two providers'
            // episode ids colliding numerically means nothing at all.
            if source == MetadataSource::Tmdb {
                for row in stored {
                    if let Some(id) = row.tmdb_episode_id {
                        index.by_identity.insert(id.to_string(), row);
                    }
                }
            }
        }
        LinkMethod::Coordinates => {
            for row in stored {
                index
                    .by_coordinate
                    .insert((row.season_number, row.episode_number), row);
            }
        }
        LinkMethod::AirDate => {
            // A day decides only when it names exactly one episode on
            // each side. Season 0 is excluded from the stored side: a
            // special sharing a day with the episode it accompanies would
            // otherwise be the answer to that day.
            let stored_days = unique_days(
                stored
                    .iter()
                    .filter(|e| e.season_number > 0)
                    .filter_map(|e| e.air_date.map(|d| (d.date(), e))),
            );
            let incoming_days = unique_days_of(
                incoming
                    .iter()
                    .filter(|(season, _)| *season > 0)
                    .filter_map(|(_, e)| e.air_date.map(time::OffsetDateTime::date)),
            );
            for (day, row) in stored_days {
                if incoming_days.contains(&day) {
                    index.by_day.insert(day, row);
                }
            }
        }
        LinkMethod::Absolute => {
            // Reachable only when the stored tree is one season: that is
            // the shape a flattened series has, and the only shape in
            // which a stored episode number *is* an absolute number.
            // Mapping 1..801 onto a seasoned tree is how a whole library
            // lands in a season that does not exist.
            let seasons: HashSet<i32> = stored
                .iter()
                .map(|e| e.season_number)
                .filter(|n| *n > 0)
                .collect();
            if seasons.len() == 1 {
                for row in stored.iter().filter(|e| e.season_number > 0) {
                    index.by_absolute.insert(row.episode_number, row);
                }
            }
        }
    }

    index
}

/// Keep only the days that name exactly one episode.
fn unique_days<'a, I>(pairs: I) -> HashMap<Date, &'a Episode>
where
    I: Iterator<Item = (Date, &'a Episode)>,
{
    let mut seen: HashMap<Date, Option<&'a Episode>> = HashMap::new();
    for (day, row) in pairs {
        seen.entry(day)
            .and_modify(|slot| *slot = None)
            .or_insert(Some(row));
    }
    seen.into_iter()
        .filter_map(|(day, row)| row.map(|r| (day, r)))
        .collect()
}

/// Same, for the incoming side, where the value is never needed.
fn unique_days_of<I>(days: I) -> HashSet<Date>
where
    I: Iterator<Item = Date>,
{
    let mut counts: HashMap<Date, u32> = HashMap::new();
    for day in days {
        *counts.entry(day).or_insert(0) += 1;
    }
    counts
        .into_iter()
        .filter_map(|(day, n)| (n == 1).then_some(day))
        .collect()
}

/// Share of a set that carries an air date, as a 0.0..=1.0 ratio.
///
/// An empty side counts as fully covered: there is nothing to be
/// suspicious of, and returning 0.0 would arm a gate over a tree that has
/// no episodes to move.
#[allow(
    clippy::cast_precision_loss,
    reason = "a ratio of episode counts; f32 is exact well past any real series"
)]
fn coverage(total: usize, dated: usize) -> f32 {
    if total == 0 {
        return 1.0;
    }
    dated as f32 / total as f32
}

pub use crate::db::library::StructureOwner;

/// Read who owns a series' shape.
///
/// # Errors
///
/// Returns [`AppError::NotFound`] for an unknown item, or
/// [`AppError::Database`] on SQL failure.
pub async fn owner(pool: &Pool, item_id: Uuid) -> Result<StructureOwner, AppError> {
    library::structure_owner(pool, item_id).await
}

/// The ordering an item is under, rebuilt from the three columns that
/// record it.
///
/// This is what a refresh has to ask the provider for. Asking for
/// [`Ordering::Default`] instead — which is what every caller did before
/// there was anything else to ask for — fetches the provider's own shape
/// and hands it to [`apply`], where the pin refuses it: a title under a
/// declared ordering could never pick up a new episode, and the failure
/// would read as the provider being wrong.
///
/// Every unreadable combination degrades to [`Ordering::Default`] rather
/// than to an error, and each one is a shape only a future brarr or a
/// hand-edited row can produce: a `Named` family with no handle names an
/// ordering nobody can fetch, and a `Manual` family with no parseable
/// recipe has no sizes to cut with. The tree that comes back is visibly
/// the provider's own, which the operator can see and re-declare.
#[must_use]
pub fn ordering_of(owner: &StructureOwner) -> Ordering {
    match owner.family {
        None | Some(OrderingFamily::Default) => Ordering::Default,
        Some(OrderingFamily::Manual) => owner
            .recipe
            .as_deref()
            .and_then(Recipe::parse)
            .map_or(Ordering::Default, |r| Ordering::Manual {
                blocks: r.blocks(),
            }),
        Some(family) => {
            owner
                .handle
                .as_deref()
                .map_or(Ordering::Default, |handle| Ordering::Named {
                    family,
                    handle: handle.into(),
                })
        }
    }
}

/// What authorises a tree write.
///
/// The gates in this module divide cleanly in two, and the division is
/// who they protect against. [`Intent::Refresh`] is a sweep, and the
/// recorded owner and the pin are exactly what stop it from moving a
/// title behind the operator's back. [`Intent::Choice`] is the operator
/// at the screen: the owner and the pin are the thing being changed, so
/// they cannot also be what refuses the change.
///
/// **Nothing else is waived.** The empty tree, the orphans, the air-date
/// coverage and the orphan-count net inside the transaction all still
/// run, because those protect *files*, and choosing an ordering is not
/// authority to unlink one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    /// A sweep refreshing the ordering already in force.
    Refresh,
    /// The operator declaring this ordering for this title.
    Choice {
        /// Whether to freeze it against future sweeps.
        pinned: bool,
        /// The sizes behind an [`Ordering::Manual`], to store so the next
        /// refresh can cut again.
        recipe: Option<Recipe>,
    },
}

/// Work out what a tree write would do, writing nothing.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn plan(
    pool: &Pool,
    item_id: Uuid,
    incoming: &SeriesTree,
) -> Result<StructurePlan, AppError> {
    let stored = library::episodes(pool, item_id).await?;
    let paired = pair(&stored, incoming);

    let grabs_per_episode = grabs::episode_grab_counts(pool, item_id).await?;
    let by_id: HashMap<Uuid, &Episode> = stored.iter().map(|e| (e.id, e)).collect();

    let orphans = paired
        .unclaimed
        .iter()
        .filter_map(|id| by_id.get(id).map(|e| (id, e)))
        .map(|(id, e)| Orphan {
            episode_id: *id,
            season: e.season_number,
            number: e.episode_number,
            title: e.title.clone(),
            grabs: grabs_per_episode.get(id).copied().unwrap_or(0),
        })
        .collect();

    let incoming_total = incoming.episode_count();
    let incoming_dated = incoming
        .seasons
        .iter()
        .flat_map(|s| &s.episodes)
        .filter(|e| e.air_date.is_some())
        .count();
    let stored_dated = stored.iter().filter(|e| e.air_date.is_some()).count();

    Ok(StructurePlan {
        item_id,
        source: incoming.source,
        added: incoming_total.saturating_sub(paired.by_coordinate.len()),
        pairs: paired.by_coordinate.into_values().collect(),
        orphans,
        air_date_coverage: (
            coverage(stored.len(), stored_dated),
            coverage(incoming_total, incoming_dated),
        ),
        packs_affected: pack_impacts(&stored, incoming, &grabs::pack_counts(pool, item_id).await?),
    })
}

/// Seasons whose pack grabs would come to mean something different.
fn pack_impacts(
    stored: &[Episode],
    incoming: &SeriesTree,
    packs: &HashMap<i32, i64>,
) -> Vec<PackImpact> {
    let mut out: Vec<PackImpact> = packs
        .iter()
        .filter_map(|(season, grabs)| {
            let was = stored.iter().filter(|e| e.season_number == *season).count();
            let now = incoming
                .seasons
                .iter()
                .filter(|s| s.number == *season)
                .map(|s| s.episodes.len())
                .sum();
            (was != now).then_some(PackImpact {
                season: *season,
                was,
                now,
                grabs: *grabs,
            })
        })
        .collect();
    out.sort_unstable_by_key(|p| p.season);
    out
}

/// Write a series' season tree, or refuse and say why.
///
/// The only production door to [`library::write_tree`]. Every gate this
/// runs is documented at the top of the module; the short version is that
/// a tree write re-points every acquisition hanging off the item, and
/// every way it can go wrong is invisible on screen.
///
/// # Errors
///
/// - [`AppError::InvalidInput`] when the tree is empty, when its source
///   is not the one the item records, when the item is pinned to a
///   different ordering, when stored episodes would be orphaned, or when
///   a move has too few air dates behind it.
/// - [`AppError::NotFound`] for an unknown item.
/// - [`AppError::Database`] on SQL failure.
pub async fn apply(pool: &Pool, item_id: Uuid, incoming: &SeriesTree) -> Result<Applied, AppError> {
    apply_with(pool, item_id, incoming, &Intent::Refresh).await
}

/// [`apply`], told who is asking.
///
/// # Errors
///
/// The same as [`apply`], except that under [`Intent::Choice`] the source
/// and the pin do not refuse — see [`Intent`] for why, and for the list
/// of what still does.
pub async fn apply_with(
    pool: &Pool,
    item_id: Uuid,
    incoming: &SeriesTree,
    intent: &Intent,
) -> Result<Applied, AppError> {
    let owner = library::structure_owner(pool, item_id).await?;
    if matches!(intent, Intent::Refresh) {
        guard_source(&owner, incoming, item_id)?;
    }

    // An empty tree is never a legal write over a live one: it prunes
    // every episode and orphans every acquisition on the item. This is
    // the same distinction `MetadataError::Empty` draws at the provider
    // boundary, restated here because a caller can build a `SeriesTree`
    // without going through a provider at all.
    if incoming.episode_count() == 0 {
        return Err(AppError::InvalidInput(format!(
            "recusado: {} devolveu uma árvore sem episódios para este título",
            incoming.source.display_name()
        )));
    }

    let plan = plan(pool, item_id, incoming).await?;
    guard_plan(&plan)?;

    let stored = library::episodes(pool, item_id).await?;
    let by_coordinate: HashMap<(i32, i32), Pairing> = plan
        .pairs
        .iter()
        .map(|p| ((p.season, p.number), *p))
        .collect();
    let stored_flags: HashMap<Uuid, bool> = stored.iter().map(|e| (e.id, e.monitored)).collect();
    let stored_seasons = library::seasons(pool, item_id).await?;
    let season_rows: HashMap<i32, (Uuid, bool)> = stored_seasons
        .iter()
        .map(|s| (s.season_number, (s.id, s.monitored)))
        .collect();

    let policy =
        library::FlagPolicy::read(pool, item_id, incoming.seasons.iter().map(|s| s.number)).await?;

    let decided = incoming
        .seasons
        .iter()
        .map(|season| {
            let known = season_rows.get(&season.number);
            library::DecidedSeason {
                id: known.map_or_else(Uuid::new_v4, |(id, _)| *id),
                number: season.number,
                // Trust the episode list over any count a provider
                // reports: the list is what the tree is built from.
                episode_count: i32::try_from(season.episodes.len()).unwrap_or(0),
                air_date: season.air_date,
                monitored: policy.for_row(season.number, season.air_date, known.map(|(_, m)| *m)),
                episodes: season
                    .episodes
                    .iter()
                    .map(|episode| {
                        let matched = by_coordinate.get(&(season.number, episode.number));
                        let id = matched.map_or_else(Uuid::new_v4, |p| p.stored_id);
                        library::DecidedEpisode {
                            id,
                            number: episode.number,
                            title: episode.title.clone(),
                            air_date: episode.air_date,
                            // Kept in step with the neutral pair while
                            // the legacy column is still read: a TMDB
                            // tree writes both, any other owner writes
                            // only the neutral one.
                            tmdb_episode_id: (incoming.source == MetadataSource::Tmdb)
                                .then(|| episode.external_id.parse::<i64>().ok())
                                .flatten(),
                            source: Some(incoming.source),
                            external_id: Some(episode.external_id.clone()),
                            absolute_number: episode.absolute_number,
                            monitored: policy.for_row(
                                season.number,
                                episode.air_date,
                                matched.and_then(|p| stored_flags.get(&p.stored_id).copied()),
                            ),
                        }
                    })
                    .collect(),
            }
        })
        .collect::<Vec<_>>();

    library::write_tree(pool, item_id, &decided).await?;

    // Recorded after the write, and never inside it: the tree is the
    // thing that must be all-or-nothing, and a stamp that outran a
    // rolled-back write would claim an ordering the rows do not have.
    match intent {
        Intent::Refresh => {
            library::record_structure(pool, item_id, incoming.source, &incoming.ordering).await?;
        }
        Intent::Choice { pinned, recipe } => {
            let rendered = recipe.as_ref().map(Recipe::render).transpose()?;
            library::set_structure_choice(
                pool,
                item_id,
                incoming.source,
                &incoming.ordering,
                rendered.as_deref(),
                *pinned,
            )
            .await?;
        }
    }

    Ok(Applied {
        reused: plan.pairs.len(),
        added: plan.added,
        packs_affected: plan.packs_affected,
    })
}

/// What an accepted write did.
#[derive(Debug, Clone)]
pub struct Applied {
    /// Stored episodes that kept their row, and so kept their files.
    pub reused: usize,
    /// Episodes the tree gained.
    pub added: usize,
    /// Season packs whose meaning changed. Reported, never blocking —
    /// it is the correction, but it has to be said rather than
    /// discovered.
    pub packs_affected: Vec<PackImpact>,
}

/// Refuse a tree the item does not own.
fn guard_source(
    owner: &StructureOwner,
    incoming: &SeriesTree,
    item_id: Uuid,
) -> Result<(), AppError> {
    if let Some(recorded) = owner.source {
        if recorded != incoming.source {
            return Err(AppError::InvalidInput(format!(
                "recusado: a estrutura de {item_id} pertence a {} e a árvore veio de {}",
                recorded.display_name(),
                incoming.source.display_name()
            )));
        }
    }

    // A pin freezes the *choice*, not the data: an ordinary refresh under
    // the ordering already in force still has to work, or a pinned title
    // could never pick up a new episode. What it refuses is a sweep
    // moving the title to a different ordering behind the operator.
    if owner.pinned {
        let same_family = owner.family == Some(incoming.ordering.family());
        let same_handle = owner.handle.as_deref() == incoming.ordering.handle();
        if !same_family || !same_handle {
            return Err(AppError::InvalidInput(format!(
                "recusado: a estrutura de {item_id} está fixada pelo operador"
            )));
        }
    }
    Ok(())
}

/// Why this plan would be refused, or `None` if it would commit.
///
/// The sentence and the gate are the same code on purpose. A screen that
/// previews a write has to say what the write would say, and the way
/// those drift is that one of them is a second copy written later — so
/// [`guard_plan`] is this function plus a `?`, and the panel reads it
/// directly rather than reconstructing the reasoning from the numbers it
/// happens to be rendering.
#[must_use]
pub fn refusal(plan: &StructurePlan) -> Option<String> {
    if !plan.orphans.is_empty() {
        return Some(format!(
            "recusado: {} episódio(s) armazenado(s) ficariam fora da árvore, levando {} aquisição(ões)",
            plan.orphans.len(),
            plan.grabs_at_risk()
        ));
    }

    if plan.moves_anything() && plan.air_dates_are_thin() {
        let (stored, incoming) = plan.air_date_coverage;
        return Some(format!(
            "recusado: a árvore renumeraria episódios com datas de exibição insuficientes \
             para confirmar o pareamento (armazenado {:.0}%, recebido {:.0}%, mínimo {:.0}%)",
            stored * 100.0,
            incoming * 100.0,
            MIN_AIR_DATE_COVERAGE * 100.0
        ));
    }
    None
}

/// Refuse a plan that would lose a file, or move rows on no evidence.
fn guard_plan(plan: &StructurePlan) -> Result<(), AppError> {
    refusal(plan).map_or(Ok(()), |why| Err(AppError::InvalidInput(why)))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert on happy paths"
)]
mod tests {
    use super::*;
    use brarr_core::{Ordering, TreeSeason};
    use time::{Duration, OffsetDateTime};

    /// A stored row, with only what the tiers read.
    fn stored(season: i32, number: i32) -> Episode {
        Episode {
            id: Uuid::new_v4(),
            tmdb_episode_id: None,
            item_id: Uuid::nil(),
            season_id: Uuid::nil(),
            season_number: season,
            episode_number: number,
            title: None,
            air_date: None,
            monitored: true,
            source: None,
            external_id: None,
            absolute_number: None,
        }
    }

    fn owned(season: i32, number: i32, id: &str) -> Episode {
        Episode {
            source: Some(MetadataSource::Tmdb),
            external_id: Some(id.to_owned()),
            ..stored(season, number)
        }
    }

    fn day(n: i64) -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::days(n)
    }

    fn incoming(season: i32, episodes: Vec<TreeEpisode>) -> TreeSeason {
        TreeSeason {
            number: season,
            air_date: None,
            episodes,
        }
    }

    fn ep(number: i32, external_id: &str) -> TreeEpisode {
        TreeEpisode {
            external_id: external_id.to_owned(),
            number,
            title: None,
            air_date: None,
            absolute_number: None,
        }
    }

    fn tree(source: MetadataSource, seasons: Vec<TreeSeason>) -> SeriesTree {
        SeriesTree {
            source,
            ordering: Ordering::Default,
            seasons,
        }
    }

    /// The tier order IS the safety argument. The Simpsons pair 801 of
    /// 801 on identity, so the absolute axis — which would map 1..801
    /// onto a season that does not exist — is never reached.
    ///
    /// The absolute numbers here are deliberately wrong by one. If the
    /// tiers ever invert, this test reports the shift rather than the
    /// order, which is the failure that actually shipped once.
    #[test]
    fn pair_prefers_identity_over_air_date_over_absolute() {
        let stored_rows: Vec<Episode> = (1..=801)
            .map(|n| Episode {
                air_date: Some(day(i64::from(n))),
                ..owned(1, n, &format!("tmdb-{n}"))
            })
            .collect();

        let incoming_eps: Vec<TreeEpisode> = (1..=801)
            .map(|n| TreeEpisode {
                air_date: Some(day(i64::from(n))),
                // One off on purpose: reaching this tier is the bug.
                absolute_number: Some(n + 1),
                ..ep(n, &format!("tmdb-{n}"))
            })
            .collect();

        let paired = pair(
            &stored_rows,
            &tree(MetadataSource::Tmdb, vec![incoming(1, incoming_eps)]),
        );

        assert_eq!(paired.by_coordinate.len(), 801);
        assert!(paired.unclaimed.is_empty());
        assert!(
            paired
                .by_coordinate
                .values()
                .all(|p| p.method == LinkMethod::Owner),
            "every episode must pair on identity, never on a weaker tier"
        );
        assert!(
            paired.by_coordinate.values().all(|p| !p.moved),
            "a refresh that changes nothing must not report a move"
        );
    }

    /// A day that names two episodes names none. Without this the
    /// arbiter picks whichever the map saw last, which is a coin toss
    /// wearing a tier's name.
    #[test]
    fn a_shared_air_date_does_not_decide() {
        let stored_rows = vec![
            Episode {
                air_date: Some(day(10)),
                ..stored(1, 1)
            },
            Episode {
                air_date: Some(day(10)),
                ..stored(1, 2)
            },
        ];
        // Coordinates deliberately do not line up, so the air-date tier
        // is the only one that could fire.
        let incoming_eps = vec![
            TreeEpisode {
                air_date: Some(day(10)),
                ..ep(7, "x-7")
            },
            TreeEpisode {
                air_date: Some(day(11)),
                ..ep(8, "x-8")
            },
        ];

        let paired = pair(
            &stored_rows,
            &tree(MetadataSource::Tvdb, vec![incoming(1, incoming_eps)]),
        );

        assert!(
            paired.by_coordinate.is_empty(),
            "a shared day must decide nothing, got {:?}",
            paired.by_coordinate
        );
        assert_eq!(paired.unclaimed.len(), 2);
    }

    /// The absolute axis is reachable only when the stored tree is one
    /// season. A seasoned tree that fell through to it would map 1..N
    /// across season boundaries.
    #[test]
    fn a_seasoned_tree_never_falls_back_to_absolute() {
        let stored_rows = vec![stored(1, 1), stored(1, 2), stored(2, 1), stored(2, 2)];
        let incoming_eps = vec![
            TreeEpisode {
                absolute_number: Some(1),
                ..ep(50, "z-1")
            },
            TreeEpisode {
                absolute_number: Some(3),
                ..ep(51, "z-3")
            },
        ];

        let paired = pair(
            &stored_rows,
            &tree(MetadataSource::Tvdb, vec![incoming(9, incoming_eps)]),
        );

        assert!(
            paired
                .by_coordinate
                .values()
                .all(|p| p.method != LinkMethod::Absolute),
            "absolute must stay unreachable on a seasoned tree"
        );
    }

    /// The same shape, flat: one stored season, so a stored episode
    /// number really is an absolute number and the tier is legitimate.
    #[test]
    fn a_flat_stored_tree_may_use_the_absolute_tier() {
        let stored_rows = vec![stored(1, 14), stored(1, 15)];
        let incoming_eps = vec![
            TreeEpisode {
                absolute_number: Some(14),
                ..ep(1, "k-14")
            },
            TreeEpisode {
                absolute_number: Some(15),
                ..ep(2, "k-15")
            },
        ];

        let paired = pair(
            &stored_rows,
            &tree(MetadataSource::Tvdb, vec![incoming(2, incoming_eps)]),
        );

        assert_eq!(paired.by_coordinate.len(), 2);
        assert!(
            paired
                .by_coordinate
                .values()
                .all(|p| p.method == LinkMethod::Absolute && p.moved),
            "a flat tree pairs on absolute, and every row moves"
        );
    }

    /// One stored row may answer for one incoming episode and no more.
    ///
    /// `StoredTree::resolve` has this hole today: an episode matching by
    /// id and another matching by coordinates can land on the same row,
    /// the second write overwrites the first, and the tree comes out one
    /// episode short with nothing pruned and nothing logged.
    #[test]
    fn a_stored_row_is_claimed_only_once() {
        // One stored row, reachable by identity AND by coordinates.
        let stored_rows = vec![owned(1, 1, "dup")];
        let incoming_eps = vec![
            // Matches by coordinates.
            ep(1, "other"),
            // Matches by identity.
            ep(2, "dup"),
        ];

        let paired = pair(
            &stored_rows,
            &tree(MetadataSource::Tmdb, vec![incoming(1, incoming_eps)]),
        );

        assert_eq!(
            paired.by_coordinate.len(),
            1,
            "one stored row cannot answer for two episodes"
        );
        // Identity runs first, so it is episode 2 that keeps the row.
        let kept = paired.by_coordinate.get(&(1, 2)).expect("identity wins");
        assert_eq!(kept.method, LinkMethod::Owner);
        assert!(paired.unclaimed.is_empty());
    }

    /// A TheTVDB-owned tree must not read `tmdb_episode_id`. The two are
    /// different namespaces and a numeric collision means nothing.
    #[test]
    fn a_foreign_owner_does_not_read_the_tmdb_episode_id() {
        let stored_rows = vec![Episode {
            tmdb_episode_id: Some(5_345_648),
            ..stored(1, 1)
        }];
        let incoming_eps = vec![ep(4, "5345648")];

        let paired = pair(
            &stored_rows,
            &tree(MetadataSource::Tvdb, vec![incoming(1, incoming_eps)]),
        );

        assert!(
            paired.by_coordinate.is_empty(),
            "a TheTVDB id must not match a TMDB one by numeric coincidence"
        );
    }

    /// The same row, under a TMDB-owned tree, is the compatibility tier
    /// that keeps every pre-identity row linked.
    #[test]
    fn a_tmdb_tree_still_matches_a_row_that_predates_the_identity_column() {
        let stored_rows = vec![Episode {
            tmdb_episode_id: Some(5_345_648),
            ..stored(1, 1)
        }];
        let incoming_eps = vec![ep(4, "5345648")];

        let paired = pair(
            &stored_rows,
            &tree(MetadataSource::Tmdb, vec![incoming(1, incoming_eps)]),
        );

        let matched = paired.by_coordinate.get(&(1, 4)).expect("id tier matches");
        assert_eq!(matched.method, LinkMethod::ExternalId);
        assert!(matched.moved, "1x01 becoming 1x04 is a move");
    }

    // ------------------------------------------------------------------
    // Against a migrated database
    // ------------------------------------------------------------------

    use crate::db::grabs::{NewGrab, Protocol};
    use crate::db::seed::Seed;
    use crate::db::{Pool as DbPool, open_memory};
    use sqlx::Row as _;

    /// A series with a tree of `shape` seasons, dated so the coverage
    /// gate is satisfied unless a test deliberately removes the dates.
    async fn series_with(pool: &DbPool, shape: &[i32], dated: bool) -> Uuid {
        let item = library::upsert(pool, &Seed::series(62_715, "Dragon Ball Super").build())
            .await
            .unwrap();
        let mut running = 0_i64;
        let seasons = shape
            .iter()
            .enumerate()
            .map(|(index, count)| {
                let number = i32::try_from(index).unwrap() + 1;
                let episodes = (1..=*count)
                    .map(|n| {
                        running += 1;
                        TreeEpisode {
                            air_date: dated.then(|| day(running)),
                            ..ep(n, &format!("tmdb-{running}"))
                        }
                    })
                    .collect();
                incoming(number, episodes)
            })
            .collect();
        apply(pool, item.id, &tree(MetadataSource::Tmdb, seasons))
            .await
            .unwrap();
        item.id
    }

    /// A provider row. The `grabs.provider_id` FK is real, so this goes
    /// through the path the app uses rather than a hand-rolled INSERT.
    async fn provider(pool: &DbPool) -> Uuid {
        let base_url = url::Url::parse("https://capybarabr.com/").unwrap();
        crate::db::providers::insert(
            pool,
            crate::db::providers::NewProvider {
                name: "capybara",
                base_url: &base_url,
                api_token: "tok",
                kind: "unit3d",
                plugin_path: None,
            },
        )
        .await
        .unwrap()
        .id
    }

    /// Reserve a grab against one episode, the way the scanner does.
    async fn grab_on(pool: &DbPool, item: Uuid, provider_id: Uuid, episode: Uuid, key: &str) {
        crate::db::grabs::reserve(
            pool,
            &NewGrab {
                item_id: item,
                episode_id: Some(episode),
                season_number: None,
                decision_id: None,
                provider_id,
                provider_name: "capybara",
                release_id_remote: key,
                release_name: key,
                download_url: None,
                protocol: Protocol::Torrent,
            },
        )
        .await
        .unwrap()
        .expect("the barrier lets a first reservation through");
    }

    /// A series of 20 in one season, cut 12 / 8 by the operator.
    ///
    /// Returns the item and the tree that cut produces, which is the
    /// tree a later refresh has to fetch again for the choice to hold.
    async fn cut_in_two(pool: &DbPool) -> (Uuid, SeriesTree, Recipe) {
        let item = series_with(pool, &[20], true).await;

        let flat = tree(
            MetadataSource::Tmdb,
            vec![incoming(
                1,
                (1..=20)
                    .map(|n| TreeEpisode {
                        air_date: Some(day(i64::from(n))),
                        ..ep(n, &format!("tmdb-{n}"))
                    })
                    .collect(),
            )],
        );
        let recipe = Recipe {
            seasons: vec![RecipeSeason {
                season: 1,
                sizes: vec![12, 8],
            }],
        };
        let declared = flat.recut(&recipe.blocks()).unwrap();

        apply_with(
            pool,
            item,
            &declared,
            &Intent::Choice {
                pinned: true,
                recipe: Some(recipe.clone()),
            },
        )
        .await
        .unwrap();

        (item, declared, recipe)
    }

    /// The operator declared an ordering; the sweep that runs every half
    /// hour must not take it back.
    ///
    /// The second half is the one that makes the pin usable rather than
    /// merely safe: a pin freezes the *choice*, not the data, so a
    /// refresh **under the ordering in force** still commits. Without
    /// it, a pinned title could never pick up a new episode.
    #[tokio::test]
    async fn a_pinned_item_is_untouched_by_a_sweep() {
        let pool = open_memory().await.unwrap();
        let (item, declared, _) = cut_in_two(&pool).await;

        let owner = library::structure_owner(&pool, item).await.unwrap();
        assert!(owner.pinned);
        assert_eq!(owner.family, Some(OrderingFamily::Manual));

        // A sweep offering the provider's own shape is refused.
        let flat = tree(
            MetadataSource::Tmdb,
            vec![incoming(
                1,
                (1..=20)
                    .map(|n| TreeEpisode {
                        air_date: Some(day(i64::from(n))),
                        ..ep(n, &format!("tmdb-{n}"))
                    })
                    .collect(),
            )],
        );
        let refused = apply(&pool, item, &flat).await.unwrap_err();
        assert!(
            matches!(&refused, AppError::InvalidInput(m) if m.contains("fixada")),
            "expected the pin to refuse, got {refused:?}"
        );

        // The shape on disk is still the operator's.
        let seasons = library::seasons(&pool, item).await.unwrap();
        assert_eq!(seasons.len(), 2);

        // And the same ordering refreshes normally.
        apply(&pool, item, &declared).await.unwrap();
        assert_eq!(library::seasons(&pool, item).await.unwrap().len(), 2);
    }

    /// The cut has to be re-applicable, not merely applied.
    ///
    /// `Ordering::Manual` carries the blocks, but a refresh starts from
    /// the provider's own tree and has to cut it again — so without the
    /// recipe stored, the sizes are gone the moment the operator closes
    /// the panel, and the next refresh silently reverts the title to
    /// TMDB's shape. `ordering_of` reading them back out of the column is
    /// what a refresh will ask for.
    #[tokio::test]
    async fn a_manual_ordering_survives_a_refresh() {
        let pool = open_memory().await.unwrap();
        let (item, _, recipe) = cut_in_two(&pool).await;

        let owner = library::structure_owner(&pool, item).await.unwrap();
        assert_eq!(
            owner.recipe.as_deref().and_then(Recipe::parse),
            Some(recipe.clone())
        );
        assert_eq!(
            ordering_of(&owner),
            Ordering::Manual {
                blocks: recipe.blocks()
            },
            "a refresh has to be able to ask for the cut, not just for TMDB's shape"
        );
    }

    /// Dragon Ball Super: TMDB flattens it into one season of 131 and
    /// TheTVDB cuts it 14 / 13 / 19 / 30 / 55, so a pack recorded against
    /// "season 1" stops meaning the whole series the instant the tree
    /// switches.
    ///
    /// **That is the correction, not a regression** — the file really
    /// does hold 14 episodes and always did. What makes it worth a
    /// number in the report is that nothing else says so: the pack keeps
    /// its row, the barrier keeps its key, and the 117 episodes it used
    /// to cover simply become missing, on a screen that was green the
    /// day before.
    #[tokio::test]
    async fn a_season_pack_impact_is_reported() {
        const SHAPE: [i32; 5] = [14, 13, 19, 30, 55];

        let pool = open_memory().await.unwrap();
        let item = series_with(&pool, &[131], true).await;
        let provider_id = provider(&pool).await;

        // A season pack, which is what `season_number` with no episode
        // means and the only thing that writes that column.
        crate::db::grabs::reserve(
            &pool,
            &NewGrab {
                item_id: item,
                episode_id: None,
                season_number: Some(1),
                decision_id: None,
                provider_id,
                provider_name: "capybara",
                release_id_remote: "dbs-s01-pack",
                release_name: "Dragon Ball Super S01 1080p",
                download_url: None,
                protocol: Protocol::Torrent,
            },
        )
        .await
        .unwrap()
        .expect("the barrier lets a first reservation through");

        let mut flat = 0_i64;
        let seasons: Vec<TreeSeason> = SHAPE
            .iter()
            .enumerate()
            .map(|(index, count)| {
                let episodes = (1..=*count)
                    .map(|n| {
                        flat += 1;
                        TreeEpisode {
                            air_date: Some(day(flat)),
                            ..ep(n, &format!("tmdb-{flat}"))
                        }
                    })
                    .collect();
                incoming(i32::try_from(index).unwrap() + 1, episodes)
            })
            .collect();

        let plan = plan(&pool, item, &tree(MetadataSource::Tmdb, seasons))
            .await
            .unwrap();

        assert_eq!(
            plan.packs_affected,
            vec![PackImpact {
                season: 1,
                was: 131,
                now: 14,
                grabs: 1,
            }],
            "the season the pack was recorded against narrows, and the report has to say so"
        );

        // The seasons the switch invents carry no pack, so they are not
        // impacts — only a season that already had one can change what
        // it means.
        assert!(plan.packs_affected.iter().all(|p| p.grabs > 0));
    }

    /// Every episode's UUID and where it sits, so a round trip can be
    /// compared against itself rather than against a hand-written list.
    async fn fingerprint(pool: &DbPool, item: Uuid) -> Vec<(Uuid, i32, i32)> {
        let mut rows: Vec<(Uuid, i32, i32)> = library::episodes(pool, item)
            .await
            .unwrap()
            .into_iter()
            .map(|e| (e.id, e.season_number, e.episode_number))
            .collect();
        rows.sort_unstable();
        rows
    }

    /// Which grab points at which episode, which is the thing a flip
    /// must not disturb and the one whose damage is invisible.
    async fn links(pool: &DbPool, item: Uuid) -> Vec<(String, Option<Uuid>)> {
        let mut rows: Vec<(String, Option<Uuid>)> =
            sqlx::query("SELECT release_id_remote, episode_id FROM grabs WHERE item_id = ?")
                .bind(item.to_string())
                .fetch_all(pool)
                .await
                .unwrap()
                .into_iter()
                .map(|r| {
                    let key: String = r.try_get("release_id_remote").unwrap();
                    let episode: Option<String> = r.try_get("episode_id").unwrap();
                    (key, episode.and_then(|e| Uuid::parse_str(&e).ok()))
                })
                .collect();
        rows.sort_unstable();
        rows
    }

    /// **The test that says the flip is safe to try.** Dragon Ball
    /// Super goes from TMDB's flat 131 to TheTVDB's 14 / 13 / 19 / 30 /
    /// 55 and back, and every row and every link survives both trips.
    ///
    /// Reversibility is what makes the whole phase a decision the
    /// operator can unmake. It rests on two things this module already
    /// does and one it now does: identity columns are `COALESCE`d, so
    /// the TMDB episode id set on the way out is still there on the way
    /// back and the return trip pairs on identity rather than on a
    /// guess; `park` vacates the number space, so a permuting rewrite
    /// does not trip the uniqueness index mid-transaction; and
    /// `Intent::Choice` lets the operator move a title the recorded
    /// owner would otherwise refuse — while every gate that protects a
    /// *file* still runs, which is why the assertion on `links` is the
    /// real one.
    /// One TheTVDB tree, under a named ordering, with stable episode ids.
    ///
    /// The ids do not change with the ordering, which is the property the
    /// whole design rests on: TheTVDB returns the *same* episodes under
    /// every season type, so two orderings of one series join on identity
    /// rather than on a guess.
    fn tvdb_tree(shape: &[i32], ordering: Ordering) -> SeriesTree {
        let mut flat = 0_i64;
        let seasons = shape
            .iter()
            .enumerate()
            .map(|(index, count)| {
                let episodes = (1..=*count)
                    .map(|n| {
                        flat += 1;
                        TreeEpisode {
                            air_date: Some(day(flat)),
                            ..ep(n, &format!("tvdb-{flat}"))
                        }
                    })
                    .collect();
                incoming(i32::try_from(index).unwrap() + 1, episodes)
            })
            .collect();
        SeriesTree {
            source: MetadataSource::Tvdb,
            ordering,
            seasons,
        }
    }

    /// **The test that says an ordering is safe to change.** Dragon Ball
    /// Super goes from TheTVDB's broadcast cut to a flat one and back,
    /// and every row and every acquisition survives both trips.
    ///
    /// This is the shape the whole refactor exists to reach: one owner,
    /// two of its own orderings, and a join on the provider's own episode
    /// ids. No air date is consulted, no absolute axis, nothing is
    /// compared across catalogues — the tier that carries it is
    /// [`LinkMethod::Owner`], and the test asserts that rather than
    /// hoping for it.
    #[tokio::test]
    async fn changing_ordering_under_one_owner_is_reversible() {
        const BROADCAST: [i32; 5] = [14, 13, 19, 30, 55];

        let pool = open_memory().await.unwrap();
        let provider_id = provider(&pool).await;
        let item = library::upsert(&pool, &Seed::series(62_715, "Dragon Ball Super").build())
            .await
            .unwrap()
            .id;

        let broadcast = tvdb_tree(&BROADCAST, Ordering::Default);
        apply_with(
            &pool,
            item,
            &broadcast,
            &Intent::Choice {
                pinned: false,
                recipe: None,
            },
        )
        .await
        .unwrap();

        // Acquisitions spread across four of the five seasons.
        let stored = library::episodes(&pool, item).await.unwrap();
        for n in [1_usize, 20, 60, 130] {
            grab_on(&pool, item, provider_id, stored[n].id, &format!("dbs-{n}")).await;
        }
        let before = fingerprint(&pool, item).await;
        let links_before = links(&pool, item).await;
        assert_eq!(before.len(), 131);

        // Out: the same 131 episodes, numbered straight through.
        let flat = tvdb_tree(
            &[131],
            Ordering::Named {
                family: OrderingFamily::Absolute,
                handle: "absolute".into(),
            },
        );
        let outbound = plan(&pool, item, &flat).await.unwrap();
        assert!(
            outbound.pairs.iter().all(|p| p.method == LinkMethod::Owner),
            "one owner's two orderings join on its own episode ids, never on a heuristic"
        );

        apply_with(
            &pool,
            item,
            &flat,
            &Intent::Choice {
                pinned: true,
                recipe: None,
            },
        )
        .await
        .unwrap();

        assert_eq!(library::seasons(&pool, item).await.unwrap().len(), 1);
        assert_eq!(
            fingerprint(&pool, item)
                .await
                .iter()
                .map(|(id, ..)| *id)
                .collect::<HashSet<_>>(),
            before.iter().map(|(id, ..)| *id).collect::<HashSet<_>>(),
            "every UUID survived the trip out"
        );
        assert_eq!(links(&pool, item).await, links_before);

        // Back.
        apply_with(
            &pool,
            item,
            &broadcast,
            &Intent::Choice {
                pinned: false,
                recipe: None,
            },
        )
        .await
        .unwrap();

        assert_eq!(
            fingerprint(&pool, item).await,
            before,
            "every UUID came home to the coordinate it started at"
        );
        assert_eq!(
            links(&pool, item).await,
            links_before,
            "and every acquisition still names the episode it always did"
        );
        assert_eq!(orphan_count(&pool).await, 0);
    }

    async fn orphan_count(pool: &DbPool) -> i64 {
        sqlx::query("SELECT count(*) AS n FROM grabs WHERE scope='episode' AND episode_id IS NULL")
            .fetch_one(pool)
            .await
            .unwrap()
            .try_get::<i64, _>("n")
            .unwrap()
    }

    /// **The most important test of the block.** A tree that re-cuts a
    /// flat season into two keeps every row, so every file stays linked.
    ///
    /// Heir to `renumbering_a_series_keeps_every_row_and_every_link`. The
    /// failure it guards renders as *complete*, not as missing: an
    /// unlinked grab covers nothing, and before `grabs.scope` existed it
    /// read as covering the whole item.
    #[tokio::test]
    async fn apply_reuses_every_episode_uuid() {
        let pool = open_memory().await.unwrap();
        let item = series_with(&pool, &[20], true).await;

        let before: HashMap<String, Uuid> = library::episodes(&pool, item)
            .await
            .unwrap()
            .into_iter()
            .filter_map(|e| e.external_id.map(|id| (id, e.id)))
            .collect();
        assert_eq!(before.len(), 20, "the identities reached the database");

        let provider_id = provider(&pool).await;
        for (n, (_, id)) in before.iter().enumerate() {
            grab_on(&pool, item, provider_id, *id, &format!("rel-{n}")).await;
        }

        // The same twenty episodes, re-cut 14 + 6 — the Dragon Ball
        // Super shape, and a permutation of the number space.
        let mut running = 0_i64;
        let recut = vec![
            incoming(
                1,
                (1..=14)
                    .map(|n| {
                        running += 1;
                        TreeEpisode {
                            air_date: Some(day(running)),
                            ..ep(n, &format!("tmdb-{running}"))
                        }
                    })
                    .collect(),
            ),
            incoming(
                2,
                (1..=6)
                    .map(|n| {
                        running += 1;
                        TreeEpisode {
                            air_date: Some(day(running)),
                            ..ep(n, &format!("tmdb-{running}"))
                        }
                    })
                    .collect(),
            ),
        ];
        apply(&pool, item, &tree(MetadataSource::Tmdb, recut))
            .await
            .unwrap();

        let after: HashMap<String, Uuid> = library::episodes(&pool, item)
            .await
            .unwrap()
            .into_iter()
            .filter_map(|e| e.external_id.map(|id| (id, e.id)))
            .collect();
        assert_eq!(after.len(), 20, "no episode was lost");
        assert_eq!(before, after, "every episode kept its row id");
        assert_eq!(orphan_count(&pool).await, 0, "no file was unlinked");
    }

    /// Two seasons swapping numbers is the case `park` exists for: SQLite
    /// checks UNIQUE per statement and has no deferred constraints, so
    /// without the `n -> -1 - n` detour the transaction aborts halfway.
    #[tokio::test]
    async fn park_survives_a_permutation() {
        let pool = open_memory().await.unwrap();
        let item = series_with(&pool, &[3, 3], true).await;

        // Season 1's episodes become season 2's and vice versa: every
        // (season, episode) pair is occupied on both sides.
        let swapped = vec![
            incoming(
                1,
                (1..=3)
                    .map(|n| TreeEpisode {
                        air_date: Some(day(i64::from(n) + 3)),
                        ..ep(n, &format!("tmdb-{}", n + 3))
                    })
                    .collect(),
            ),
            incoming(
                2,
                (1..=3)
                    .map(|n| TreeEpisode {
                        air_date: Some(day(i64::from(n))),
                        ..ep(n, &format!("tmdb-{n}"))
                    })
                    .collect(),
            ),
        ];
        apply(&pool, item, &tree(MetadataSource::Tmdb, swapped))
            .await
            .unwrap();

        let rows = library::episodes(&pool, item).await.unwrap();
        assert_eq!(rows.len(), 6);
        let placed: HashMap<String, (i32, i32)> = rows
            .into_iter()
            .filter_map(|e| {
                e.external_id
                    .map(|id| (id, (e.season_number, e.episode_number)))
            })
            .collect();
        assert_eq!(placed.get("tmdb-1"), Some(&(2, 1)), "it crossed over");
        assert_eq!(placed.get("tmdb-4"), Some(&(1, 1)), "and so did its twin");
    }

    /// A tree from a source the item does not own is refused, because
    /// `library_seasons` has no origin column: two sweeps would overwrite
    /// each other in silence and the prune would delete whatever went
    /// unclaimed.
    #[tokio::test]
    async fn a_tree_from_the_wrong_source_is_refused() {
        let pool = open_memory().await.unwrap();
        let item = series_with(&pool, &[3], true).await;

        let err = apply(
            &pool,
            item,
            &tree(
                MetadataSource::Tvdb,
                vec![incoming(1, vec![ep(1, "tvdb-1")])],
            ),
        )
        .await
        .expect_err("a TheTVDB tree over a TMDB-owned item must be refused");
        assert!(
            err.to_string().contains("TheTVDB"),
            "the refusal names the source that tried: {err}"
        );
        assert_eq!(
            library::episodes(&pool, item).await.unwrap().len(),
            3,
            "and it wrote nothing"
        );
    }

    /// A series nobody has claimed is adopted rather than refused. Every
    /// title catalogued since the identity migration reads this way,
    /// because `library::upsert` does not write the column.
    #[tokio::test]
    async fn an_unclaimed_series_is_adopted_and_recorded() {
        let pool = open_memory().await.unwrap();
        let item = library::upsert(&pool, &Seed::series(1399, "Game of Thrones").build())
            .await
            .unwrap();
        assert_eq!(
            library::structure_owner(&pool, item.id)
                .await
                .unwrap()
                .source,
            None,
            "upsert leaves the column blank"
        );

        apply(
            &pool,
            item.id,
            &tree(
                MetadataSource::Tmdb,
                vec![incoming(1, vec![ep(1, "tmdb-1")])],
            ),
        )
        .await
        .unwrap();

        let owner = library::structure_owner(&pool, item.id).await.unwrap();
        assert_eq!(owner.source, Some(MetadataSource::Tmdb));
        assert_eq!(owner.family, Some(brarr_core::OrderingFamily::Default));
    }

    /// An empty answer must never be written over a live tree: it prunes
    /// every episode and orphans every acquisition on the item.
    #[tokio::test]
    async fn an_empty_tree_is_refused() {
        let pool = open_memory().await.unwrap();
        let item = series_with(&pool, &[3], true).await;

        let err = apply(&pool, item, &tree(MetadataSource::Tmdb, vec![]))
            .await
            .expect_err("an empty tree is not a legal write");
        assert!(err.to_string().contains("sem episódios"), "{err}");
        assert_eq!(library::episodes(&pool, item).await.unwrap().len(), 3);
    }

    /// The gate the orphan check cannot stand in for.
    ///
    /// The Kaiju No. 8 shape. A flat stored season is re-cut into a
    /// numbered one and the only evidence available is the absolute axis
    /// — which is exactly the axis that lies there, because TheTVDB gives
    /// absolute 13 to a special and every episode after it carries a
    /// number one higher than the season implies.
    ///
    /// The point of the test is what the *other* gates do: the shift
    /// consumes every row on **both** sides, so `orphans` comes back
    /// empty and the orphan gate passes. Nothing else would stop it, and
    /// the result would be every episode covered by its neighbour's file
    /// — a library that renders complete and is wrong.
    #[tokio::test]
    async fn a_uniform_shift_is_refused_when_air_dates_are_thin() {
        let pool = open_memory().await.unwrap();
        // No dates and no identities: the pre-backfill shape, and the
        // condition under test.
        let item = library::upsert(&pool, &Seed::series(240_411, "Kaiju No. 8").build())
            .await
            .unwrap();
        library::sync_seasons(
            &pool,
            item.id,
            &[crate::db::library::NewSeason {
                season_number: 1,
                episode_count: 12,
                air_date: None,
                episodes: (1..=12).map(crate::db::seed::episode).collect(),
            }],
        )
        .await
        .unwrap();

        // Season 2 of twelve, reachable only through the absolute axis.
        let recut = vec![incoming(
            2,
            (1..=12)
                .map(|n| TreeEpisode {
                    absolute_number: Some(n),
                    ..ep(n, &format!("tvdb-{n}"))
                })
                .collect(),
        )];

        let plan = plan(&pool, item.id, &tree(MetadataSource::Tmdb, recut.clone()))
            .await
            .unwrap();
        assert!(
            plan.orphans.is_empty(),
            "the shift consumes both sides, so the orphan gate cannot see it: {:?}",
            plan.orphans
        );
        assert!(plan.moves_anything(), "and yet every row moves");
        assert!(
            plan.pairs.iter().all(|p| p.method == LinkMethod::Absolute),
            "on the one axis that lies here"
        );

        let err = apply(&pool, item.id, &tree(MetadataSource::Tmdb, recut))
            .await
            .expect_err("a move with no evidence must be refused");
        assert!(err.to_string().contains("datas de exibição"), "{err}");
    }

    /// The same re-cut, with air dates, has evidence and commits — and it
    /// pairs on the date rather than on the absolute axis. Without this
    /// the gate above could be passing for the wrong reason.
    #[tokio::test]
    async fn the_same_recut_commits_once_the_dates_are_there() {
        let pool = open_memory().await.unwrap();
        let item = library::upsert(&pool, &Seed::series(240_411, "Kaiju No. 8").build())
            .await
            .unwrap();
        library::sync_seasons(
            &pool,
            item.id,
            &[crate::db::library::NewSeason {
                season_number: 1,
                episode_count: 12,
                air_date: None,
                episodes: (1..=12)
                    .map(|n| crate::db::library::NewEpisode {
                        air_date: Some(day(i64::from(n))),
                        ..crate::db::seed::episode(n)
                    })
                    .collect(),
            }],
        )
        .await
        .unwrap();

        let recut = vec![incoming(
            2,
            (1..=12)
                .map(|n| TreeEpisode {
                    air_date: Some(day(i64::from(n))),
                    absolute_number: Some(n),
                    ..ep(n, &format!("tvdb-{n}"))
                })
                .collect(),
        )];
        let plan = plan(&pool, item.id, &tree(MetadataSource::Tmdb, recut.clone()))
            .await
            .unwrap();
        assert!(
            plan.pairs.iter().all(|p| p.method == LinkMethod::AirDate),
            "the date outranks the absolute axis"
        );

        apply(&pool, item.id, &tree(MetadataSource::Tmdb, recut))
            .await
            .unwrap();
        assert_eq!(library::episodes(&pool, item.id).await.unwrap().len(), 12);
    }

    /// A stored episode the incoming tree drops, carrying a file, stops
    /// the write before the transaction opens.
    #[tokio::test]
    async fn a_plan_with_orphans_is_refused() {
        let pool = open_memory().await.unwrap();
        let item = series_with(&pool, &[3], true).await;

        let doomed = library::episodes(&pool, item)
            .await
            .unwrap()
            .into_iter()
            .find(|e| e.episode_number == 3)
            .unwrap();
        let provider_id = provider(&pool).await;
        grab_on(&pool, item, provider_id, doomed.id, "rel-3").await;

        // Two of the three come back; the third simply is not there.
        let shrunk = vec![incoming(
            1,
            (1..=2)
                .map(|n| TreeEpisode {
                    air_date: Some(day(i64::from(n))),
                    ..ep(n, &format!("tmdb-{n}"))
                })
                .collect(),
        )];

        let plan = plan(&pool, item, &tree(MetadataSource::Tmdb, shrunk.clone()))
            .await
            .unwrap();
        assert_eq!(plan.orphans.len(), 1);
        assert_eq!(plan.grabs_at_risk(), 1, "and the plan says what it costs");

        apply(&pool, item, &tree(MetadataSource::Tmdb, shrunk))
            .await
            .expect_err("dropping an episode that holds a file is refused");
        assert_eq!(orphan_count(&pool).await, 0);
    }
}
