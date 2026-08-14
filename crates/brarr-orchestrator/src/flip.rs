//! The one-time move from a translation to a declared owner.
//!
//! Everything in this module reads `library_episode_numbering` and the
//! three `search_*` columns on `library_items` and writes nothing of its
//! own, which is what makes it the phase that can be run, read and
//! reconsidered before anything moves. It exists to answer one question
//! per title — *where does this one go, and what would happen if it
//! went there* — and it dies with the contraction that removes the
//! columns it reads.
//!
//! ## Why a destination has to be computed rather than typed in
//!
//! The operator settled the numbering of roughly fifteen titles by hand,
//! and the record of each decision is spread over three columns and 833
//! rows: `search_numbering_source` says *who* decided, `search_group_id`
//! says *which* ordering, and the rows say what the ordering does. None
//! of the three survives the contraction, and re-deciding fifteen titles
//! by hand is the work nobody does — the reason `20260813130000` exists
//! at all is that two titles got a numbering in a week when fifteen
//! needed one.
//!
//! ## What each legacy value becomes, and why
//!
//! | was | becomes | pinned |
//! |---|---|---|
//! | `'arr'`, `'tvdb'` | TheTVDB, its own ordering | no |
//! | `'tmdb'` | TMDB, the episode group it names | yes |
//! | `'manual'` | TMDB, the blocks re-declared | yes |
//! | `'off'` | TMDB, its own ordering | yes |
//! | `NULL` | nothing moves | — |
//!
//! The pin column is the interesting half, and it is not decoration. The
//! two automatic values were *derived* — a sweep wrote them and a later
//! sweep was allowed to replace them — so they arrive unpinned and the
//! ordinary refresh keeps doing its job. The three operator values were
//! decisions a sweep was never allowed to touch, and `'off'` in
//! particular exists **only** because NULL would have been undone by the
//! next cycle. Arriving unpinned would undo it now, one schema later,
//! which is the same defect wearing the new vocabulary.

use std::collections::BTreeMap;

use brarr_core::{MediaType, MetadataSource, Ordering, OrderingFamily};
use uuid::Uuid;

use crate::db::{Pool, episode_numbering, item_ids, library};
use crate::error::AppError;
use crate::metadata::registry::Registry;
use crate::structure::{self, Recipe, RecipeSeason, StructurePlan};

/// Where one title's numbering goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    /// Who will own the shape.
    pub source: MetadataSource,
    /// The ordering to ask that source for.
    pub ordering: Ordering,
    /// Whether a sweep may move it again.
    pub pinned: bool,
    /// The sizes behind an [`Ordering::Manual`], to store so the next
    /// refresh can cut again.
    pub recipe: Option<Recipe>,
}

/// What reading one title's legacy numbering produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Nobody ever decided a numbering here, and the item already reads
    /// as TMDB under its own ordering. There is nothing to move.
    Untouched,
    /// Where it goes.
    Move(Box<Destination>),
    /// The legacy row does not read as a destination, and why.
    ///
    /// Reported rather than skipped, for the reason `AxisRejection`
    /// exists: a title that silently fails to flip goes on searching
    /// under coordinates nothing will carry, with nothing on any screen
    /// to say so.
    Unreadable(String),
}

/// Read one title's legacy numbering and say where it goes.
///
/// Writes nothing.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn destination(pool: &Pool, item_id: Uuid) -> Result<Verdict, AppError> {
    let Some(legacy) = episode_numbering::source(pool, item_id).await? else {
        return Ok(Verdict::Untouched);
    };
    let handle = episode_numbering::active_group(pool, item_id)
        .await?
        .map(|(id, _)| id);

    let recipe = if matches!(legacy, episode_numbering::Source::Manual) {
        Some(declared_blocks(pool, item_id).await?)
    } else {
        None
    };

    Ok(destination_of(legacy, handle.as_deref(), recipe))
}

/// The mapping itself, with nothing to read.
///
/// An exhaustive `match` and not a lookup table: a sixth legacy value
/// would be a compile error here, which is the only kind of enumeration
/// this repository has found trustworthy.
#[must_use]
fn destination_of(
    legacy: episode_numbering::Source,
    handle: Option<&str>,
    recipe: Option<Recipe>,
) -> Verdict {
    let moved = |source, ordering, pinned, recipe| {
        Verdict::Move(Box::new(Destination {
            source,
            ordering,
            pinned,
            recipe,
        }))
    };

    match legacy {
        // Both were derived from the same numbering — the Sonarr's is
        // TheTVDB's — so both land on the source that publishes it, and
        // neither is pinned: a sweep wrote them and a sweep may keep
        // them current.
        episode_numbering::Source::Arr | episode_numbering::Source::Tvdb => {
            moved(MetadataSource::Tvdb, Ordering::Default, false, None)
        }

        // An episode group the operator picked from the panel. The hex
        // id is what selects it and is the half that must survive; the
        // family is brarr's own word for the ordering and was never
        // recorded, so `Alternate` is the honest reading of "the
        // operator chose a group that renumbers" — and the panel names
        // the real one the moment it is opened, because `variants()`
        // reports it.
        episode_numbering::Source::Tmdb => match handle {
            Some(handle) if !handle.trim().is_empty() => moved(
                MetadataSource::Tmdb,
                Ordering::Named {
                    family: OrderingFamily::Alternate,
                    handle: handle.into(),
                },
                true,
                None,
            ),
            _ => Verdict::Unreadable(
                "a numeração veio de um agrupamento do TMDB, mas o título não guarda \
                 qual — escolha a ordenação de novo no painel"
                    .to_owned(),
            ),
        },

        // Blocks the operator typed. The tree they were cut from is
        // TMDB's, so TMDB keeps the item and only the cut is declared.
        episode_numbering::Source::Manual => match recipe {
            Some(recipe) if !recipe.seasons.is_empty() => moved(
                MetadataSource::Tmdb,
                Ordering::Manual {
                    blocks: recipe.blocks(),
                },
                true,
                Some(recipe),
            ),
            _ => Verdict::Unreadable(
                "a numeração foi declarada em blocos, mas não há linhas de onde \
                 reconstruí-los — declare os blocos de novo no painel"
                    .to_owned(),
            ),
        },

        // "Leave it alone" was itself a decision, and it had to be
        // spelled `'off'` rather than NULL precisely because NULL got
        // undone by the next sweep. It arrives pinned or the sweep undoes
        // it again.
        episode_numbering::Source::Off => {
            moved(MetadataSource::Tmdb, Ordering::Default, true, None)
        }
    }
}

/// What a dry run found about one title.
#[derive(Debug, Clone)]
pub struct Preview {
    /// The title this is about.
    pub item_id: Uuid,
    /// Its name, so a report names something a human recognises.
    pub title: String,
    /// What was found.
    pub outcome: Outcome,
}

/// One title's verdict, with the plan when there is one.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// Nobody decided a numbering here. No network call was made.
    Untouched,
    /// It has somewhere to go and cannot get there, and why.
    ///
    /// A blocked title is the whole reason to run this before writing
    /// anything: it is a title whose searches go on missing, and the
    /// fix — an id it does not carry, a credential that is not
    /// configured — is one nobody discovers from a failed grab.
    Blocked {
        /// Where it was going, when that much is known.
        destination: Option<Box<Destination>>,
        /// What stopped it, in the operator's language.
        reason: String,
    },
    /// It can move, and this is what moving would do.
    Ready(Box<Ready>),
}

/// A title that can move.
#[derive(Debug, Clone)]
pub struct Ready {
    /// Where it goes.
    pub destination: Destination,
    /// What the tree write would do, computed against the real tree.
    pub plan: StructurePlan,
}

impl Ready {
    /// Whether the write would be accepted as it stands.
    ///
    /// The same two questions `guard_plan` asks, asked here so the
    /// report can rank a batch without attempting one. It is deliberately
    /// **not** a copy of the gate — it calls nothing and writes nothing,
    /// and the gate remains the thing that actually refuses.
    #[must_use]
    pub fn would_commit(&self) -> bool {
        self.plan.orphans.is_empty()
            && !(self.plan.moves_anything() && self.plan.air_dates_are_thin())
    }
}

/// Work out what flipping one title would do, writing nothing.
///
/// Makes at most one provider call, and none at all for a title nobody
/// decided a numbering for — which on this catalogue is roughly 165 of
/// 180, and is what makes running the whole batch cheap.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure. A provider that
/// refuses or cannot be reached is reported as [`Outcome::Blocked`]
/// rather than raised: one unreachable title must not end a batch of a
/// hundred and eighty.
pub async fn preview(
    pool: &Pool,
    registry: &Registry,
    item: &library::LibraryItem,
) -> Result<Preview, AppError> {
    let done = |outcome| {
        Ok(Preview {
            item_id: item.id,
            title: item.title.clone(),
            outcome,
        })
    };

    if item.media_type != MediaType::Tv {
        return done(Outcome::Untouched);
    }

    let destination = match destination(pool, item.id).await? {
        Verdict::Untouched => return done(Outcome::Untouched),
        Verdict::Unreadable(reason) => {
            return done(Outcome::Blocked {
                destination: None,
                reason,
            });
        }
        Verdict::Move(d) => *d,
    };

    // The id the destination's own provider answers to. Never guessed:
    // a series brarr holds only under TMDB has no TheTVDB id to search
    // with either, and saying so is the report's job.
    let ids = item_ids::for_item(pool, item.id).await?;
    let Some(known) = ids
        .iter()
        .find(|stored| stored.id.source() == destination.source)
    else {
        let reason = format!(
            "o título não guarda um id da {} — resolva o id antes de trocar a fonte",
            destination.source.display_name()
        );
        return done(Outcome::Blocked {
            destination: Some(Box::new(destination)),
            reason,
        });
    };

    let provider = match registry.require(destination.source) {
        Ok(p) => p,
        Err(e) => {
            return done(Outcome::Blocked {
                destination: Some(Box::new(destination)),
                reason: e.to_string(),
            });
        }
    };

    let incoming = match provider.tree(&known.id, &destination.ordering).await {
        Ok(tree) => tree,
        Err(e) => {
            return done(Outcome::Blocked {
                destination: Some(Box::new(destination)),
                reason: e.to_string(),
            });
        }
    };

    let plan = structure::plan(pool, item.id, &incoming).await?;
    done(Outcome::Ready(Box::new(Ready { destination, plan })))
}

/// The same, over the whole catalogue.
///
/// Sequential on purpose. The point of this pass is a report an operator
/// reads before deciding, not throughput, and firing a hundred and
/// eighty concurrent requests at two metadata providers is the way to
/// get rate-limited into a report full of `Blocked` rows that say
/// nothing about the titles.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn preview_all(pool: &Pool, registry: &Registry) -> Result<Vec<Preview>, AppError> {
    let mut out = Vec::new();
    for item in library::list(pool).await? {
        out.push(preview(pool, registry, &item).await?);
    }
    Ok(out)
}

/// Rebuild the sizes an operator typed, from the rows they produced.
///
/// The recipe column did not exist when these were declared, so the only
/// record of a hand-declared cut is its result: one row per canonical
/// episode, carrying the block it landed in. Counting rows per block,
/// per canonical season, in order, is the recipe — `rows_from_blocks`
/// wrote exactly that and nothing else.
async fn declared_blocks(pool: &Pool, item_id: Uuid) -> Result<Recipe, AppError> {
    let rows = episode_numbering::for_item(pool, item_id).await?;

    // Two nested ordered maps rather than a sort at the end: the sizes
    // are the *count per block* and the blocks have to come out in the
    // order they were declared, which is their season number.
    let mut per_season: BTreeMap<i32, BTreeMap<i32, u32>> = BTreeMap::new();
    for ((canonical_season, _), group) in rows {
        *per_season
            .entry(canonical_season)
            .or_default()
            .entry(group.season)
            .or_insert(0) += 1;
    }

    Ok(Recipe {
        seasons: per_season
            .into_iter()
            .map(|(season, blocks)| RecipeSeason {
                season,
                sizes: blocks.into_values().collect(),
            })
            .collect(),
    })
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert on happy paths"
)]
mod tests {
    use super::*;
    use crate::db::open_memory;
    use crate::db::seed::Seed;

    fn moved(v: &Verdict) -> &Destination {
        match v {
            Verdict::Move(d) => Some(&**d),
            _ => None,
        }
        .expect("expected a destination")
    }

    /// The guard the phase turns on: every legacy value has somewhere to
    /// go, and the `match` in `destination_of` is what enforces it — a
    /// sixth variant does not compile.
    ///
    /// What this test adds on top is *which* destination, and above all
    /// the pin. Three of the five values were operator decisions no
    /// sweep was ever allowed to touch; arriving unpinned would hand
    /// them straight back to the sweep, which is the defect `'off'` was
    /// invented to avoid, one schema later.
    #[test]
    fn every_legacy_numbering_source_has_a_destination() {
        use episode_numbering::Source as Legacy;

        let recipe = Recipe {
            seasons: vec![RecipeSeason {
                season: 1,
                sizes: vec![12, 13],
            }],
        };

        let cases = [
            (Legacy::Arr, None, None),
            (Legacy::Tvdb, None, None),
            (Legacy::Tmdb, Some("1a2b3c"), None),
            (Legacy::Manual, None, Some(recipe.clone())),
            (Legacy::Off, None, None),
        ];

        for (legacy, handle, recipe) in cases {
            let verdict = destination_of(legacy, handle, recipe);
            assert!(
                matches!(verdict, Verdict::Move(_)),
                "{legacy:?} has nowhere to go: {verdict:?}"
            );
        }

        // Derived, so a sweep keeps them current.
        for legacy in [Legacy::Arr, Legacy::Tvdb] {
            let d = destination_of(legacy, None, None);
            let d = moved(&d);
            assert_eq!(d.source, MetadataSource::Tvdb);
            assert_eq!(d.ordering, Ordering::Default);
            assert!(!d.pinned, "{legacy:?} was derived and stays derivable");
        }

        // Decided, so no sweep may take them back.
        let group = destination_of(Legacy::Tmdb, Some("1a2b3c"), None);
        let group = moved(&group);
        assert_eq!(group.source, MetadataSource::Tmdb);
        assert_eq!(group.ordering.handle(), Some("1a2b3c"));
        assert!(group.pinned);

        let manual = destination_of(Legacy::Manual, None, Some(recipe.clone()));
        let manual = moved(&manual);
        assert_eq!(manual.source, MetadataSource::Tmdb);
        assert_eq!(manual.ordering.family(), OrderingFamily::Manual);
        assert_eq!(manual.recipe.as_ref(), Some(&recipe));
        assert!(manual.pinned);

        let off = destination_of(Legacy::Off, None, None);
        let off = moved(&off);
        assert_eq!(off.source, MetadataSource::Tmdb);
        assert_eq!(off.ordering, Ordering::Default);
        assert!(
            off.pinned,
            "'off' was invented because NULL got undone by the next sweep"
        );
    }

    /// A legacy row that names an ordering nothing can fetch is
    /// **reported**, not skipped. A title that quietly fails to move
    /// goes on searching under coordinates no release carries.
    #[test]
    fn a_legacy_row_that_names_nothing_is_reported() {
        use episode_numbering::Source as Legacy;

        assert!(matches!(
            destination_of(Legacy::Tmdb, None, None),
            Verdict::Unreadable(_)
        ));
        assert!(matches!(
            destination_of(Legacy::Tmdb, Some("  "), None),
            Verdict::Unreadable(_)
        ));
        assert!(matches!(
            destination_of(Legacy::Manual, None, None),
            Verdict::Unreadable(_)
        ));
        assert!(matches!(
            destination_of(
                Legacy::Manual,
                None,
                Some(Recipe {
                    seasons: Vec::new()
                })
            ),
            Verdict::Unreadable(_)
        ));
    }

    /// Nobody decided anything, and the item already reads as TMDB under
    /// its own ordering — so there is nothing for the flip to do, and
    /// saying so is different from saying it failed.
    #[tokio::test]
    async fn a_title_nobody_decided_is_left_alone() {
        let pool = open_memory().await.unwrap();
        let item = crate::db::library::upsert(&pool, &Seed::series(1, "Sem decisão").build())
            .await
            .unwrap();

        assert_eq!(
            destination(&pool, item.id).await.unwrap(),
            Verdict::Untouched
        );
    }

    /// Solo Leveling: one TMDB season of 25, `S01E01`–`S01E12` and
    /// `S02E01`–`S02E13` in every release, and the sizes the operator
    /// typed were never stored — only the 25 rows they produced.
    ///
    /// Counting those rows back into `12, 13` is what lets the recipe
    /// column be filled for a title that predates it.
    #[tokio::test]
    async fn the_blocks_an_operator_typed_are_read_back_out_of_the_rows() {
        let pool = open_memory().await.unwrap();
        let item =
            crate::db::library::upsert(&pool, &Seed::series(127_532, "Solo Leveling").build())
                .await
                .unwrap();

        let blocks = [
            episode_numbering::Block {
                canonical_season: 1,
                first_episode: 1,
                last_episode: 12,
                season: 1,
            },
            episode_numbering::Block {
                canonical_season: 1,
                first_episode: 13,
                last_episode: 25,
                season: 2,
            },
        ];
        episode_numbering::apply_manual(&pool, item.id, &blocks)
            .await
            .unwrap();

        let verdict = destination(&pool, item.id).await.unwrap();
        let d = moved(&verdict);
        assert_eq!(
            d.recipe,
            Some(Recipe {
                seasons: vec![RecipeSeason {
                    season: 1,
                    sizes: vec![12, 13],
                }],
            })
        );
        assert_eq!(
            d.ordering,
            Ordering::Manual {
                blocks: vec![
                    brarr_core::Block {
                        season: 1,
                        size: 12
                    },
                    brarr_core::Block {
                        season: 1,
                        size: 13
                    },
                ],
            }
        );
        assert!(d.pinned);
    }
}
