//! Fetch the tree a title is actually under — and, for a title that has
//! none yet, decide who it should come from.
//!
//! Every refresh in brarr used to ask TMDB for `Ordering::Default`,
//! which was right while that was the only ordering there was. It stops
//! being right the moment a title has a declared structure: the sweep
//! fetches the provider's own shape, hands it to [`crate::structure::
//! apply`], and the gates there refuse it — the source no longer
//! matches, or the pin does not. A title that has just been corrected
//! would then never pick up a new episode, and the failure would read as
//! the provider being wrong rather than as the caller asking the wrong
//! question.
//!
//! So the question is asked from the item: who owns the shape, and under
//! what ordering. `library_items` records both, plus the recipe behind a
//! hand-declared cut, and [`crate::structure::ordering_of`] rebuilds the
//! `Ordering` from the three columns.
//!
//! ## An unclaimed title is decided, not assumed
//!
//! `structure_source` is written by [`crate::structure::apply`] on every
//! accepted write, so "no recorded owner" means exactly one thing: this
//! series has never had a tree. That is the one moment the choice is
//! free — nothing is linked to a coordinate yet, so nothing can be
//! unlinked by making it — and it is the moment [`born`] uses.
//!
//! **The order is TheTVDB first, and that is the whole point of this
//! module.** The scene numbers releases the way TheTVDB does: Dragon
//! Ball Super is one season of 131 on TMDB and 14/13/19/30/55 on
//! TheTVDB, and every release names the second. A series born under that
//! shape needs no translation between what brarr stores and what a
//! tracker will answer to — the search, the pairing and the destination
//! path are all already in the coordinates the release uses. The
//! translation table is not improved by this; it stops having anything
//! to do.

use std::sync::Arc;

use brarr_core::{
    MediaType, MetadataError, MetadataProvider, MetadataSource, Ordering, SeriesTree,
};
use uuid::Uuid;

use crate::db::item_ids::StoredId;
use crate::db::{Pool, item_ids};
use crate::error::AppError;
use crate::metadata::registry::Registry;
use crate::structure;

/// Where a series' shape comes from when nobody has said yet.
///
/// `None` is "never a structure owner": IMDb is a namespace brarr stores
/// ids in and never calls, so it has no tree to offer. The rest is a
/// preference, and TheTVDB leads it for the reason in the module docs.
///
/// An exhaustive `match` rather than an array literal, for the same
/// reason [`MetadataSource::all`] is derived from one: a provider added
/// without a place in this order has to be given one by the compiler,
/// not discovered missing on the day a title is born under the wrong
/// shape — a defect that renders as a green library, because the tree is
/// complete and merely numbered the way nobody else numbers it.
const fn structure_rank(source: MetadataSource) -> Option<u8> {
    match source {
        MetadataSource::Tvdb => Some(0),
        MetadataSource::Tmdb => Some(1),
        MetadataSource::Imdb => None,
    }
}

/// The tree for `item_id`, from whoever owns its shape.
///
/// # Errors
///
/// - [`AppError::NotFound`] for an unknown item.
/// - [`AppError::InvalidInput`] when the item carries no id the owning
///   source answers to. Never guessed: a series brarr holds only under
///   TMDB has no TheTVDB id to fetch with either, and inventing one is
///   how a whole tree lands on the wrong show.
/// - [`AppError::Metadata`] when the provider is unconfigured, refuses,
///   or cannot be reached.
/// - [`AppError::Database`] on SQL failure.
pub async fn tree(pool: &Pool, registry: &Registry, item_id: Uuid) -> Result<SeriesTree, AppError> {
    let owner = structure::owner(pool, item_id).await?;
    let ids = item_ids::for_item(pool, item_id).await?;

    match owner.source {
        Some(source) => from(registry, source, &ids, &structure::ordering_of(&owner)).await,
        None => born(registry, &ids).await,
    }
}

/// The tree from the source the item records, under the ordering it
/// records.
async fn from(
    registry: &Registry,
    source: MetadataSource,
    ids: &[StoredId],
    ordering: &Ordering,
) -> Result<SeriesTree, AppError> {
    let known = ids
        .iter()
        .find(|stored| stored.id.source() == source)
        .ok_or_else(|| {
            AppError::InvalidInput(format!(
                "o título não guarda um id da {}, que é quem tem a estrutura dele",
                source.display_name()
            ))
        })?;

    let provider = registry.require(source)?;
    // Asked before dispatch, so `MetadataError::Unsupported` stays a bug
    // report rather than a routine return — the same rule the registry
    // applies when it filters.
    if !provider.capabilities().structure.covers(MediaType::Tv) {
        return Err(AppError::Metadata(MetadataError::Unsupported {
            origin: source,
            capability: "tree",
            media: MediaType::Tv,
        }));
    }

    Ok(provider.tree(&known.id, ordering).await?)
}

/// The tree for a series that has never had one, from the first source
/// in [`structure_rank`] order that can answer for it.
///
/// ## Why a failure does not always move to the next candidate
///
/// Walking on is only sound when the current source *said no*: it was
/// asked, and it holds nothing under that id. A credential nobody pasted
/// and a tracker that timed out both mean nobody looked, and treating
/// those as "TheTVDB does not have it" would let a transient condition
/// pick the owner — permanently, because [`crate::structure::apply`]
/// writes the owner down and only the operator changes it afterwards.
/// So [`MetadataError::is_absence`] is what advances the walk, and
/// anything else propagates: the add fails, the operator sees why, and
/// the next sweep tries again with nothing written.
///
/// The same reasoning covers an id brarr does not hold. That is not a
/// failure at all — there is no request to make — so the candidate is
/// skipped without a word beyond a debug line.
async fn born(registry: &Registry, ids: &[StoredId]) -> Result<SeriesTree, AppError> {
    let mut candidates: Vec<(u8, &Arc<dyn MetadataProvider>)> = registry
        .for_structure(MediaType::Tv)
        .filter_map(|provider| structure_rank(provider.source()).map(|rank| (rank, provider)))
        .collect();
    candidates.sort_by_key(|(rank, _)| *rank);

    let mut refused: Option<MetadataError> = None;
    for (_, provider) in candidates {
        let source = provider.source();
        let Some(known) = ids.iter().find(|stored| stored.id.source() == source) else {
            tracing::debug!(
                target: "brarr_orchestrator::metadata",
                %source,
                "no id for this source; it cannot own the shape of a title brarr cannot name to it"
            );
            continue;
        };

        match provider.tree(&known.id, &Ordering::Default).await {
            Ok(tree) => return Ok(tree),
            Err(e) if e.is_absence() => {
                tracing::info!(
                    target: "brarr_orchestrator::metadata",
                    %source, error = %e,
                    "source has no tree for this title; trying the next one"
                );
                refused = Some(e);
            }
            Err(e) => return Err(AppError::Metadata(e)),
        }
    }

    Err(refused.map_or_else(
        || {
            AppError::InvalidInput(
                "nenhuma fonte de estrutura configurada conhece este título — \
                 confira as credenciais de metadados em /settings"
                    .to_owned(),
            )
        },
        AppError::Metadata,
    ))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert on happy paths"
)]
mod tests {
    use super::*;
    use brarr_core::{
        Capabilities, CredentialField, ExternalId, MediaSupport, MetaFuture, StructureVariant,
        TreeEpisode, TreeSeason,
    };

    use crate::db::open_memory;
    use crate::db::seed::Seed;

    /// A provider that answers whatever the test told it to.
    ///
    /// The two real implementations talk to somebody else's API over the
    /// network; what this module decides — who is asked, in what order,
    /// and what a refusal means — is worth pinning without either.
    struct Fake {
        source: MetadataSource,
        answer: Result<SeriesTree, MetadataError>,
    }

    impl Fake {
        /// A source that answers with a one-season tree naming itself.
        fn answering(source: MetadataSource) -> Arc<dyn MetadataProvider> {
            Arc::new(Self {
                source,
                answer: Ok(SeriesTree {
                    source,
                    ordering: Ordering::Default,
                    seasons: vec![TreeSeason {
                        number: 1,
                        air_date: None,
                        episodes: vec![TreeEpisode {
                            external_id: "e1".to_owned(),
                            number: 1,
                            title: Some("Pilot".to_owned()),
                            air_date: None,
                            absolute_number: None,
                        }],
                    }],
                }),
            })
        }

        /// A source that refuses with `error`.
        fn refusing(source: MetadataSource, error: MetadataError) -> Arc<dyn MetadataProvider> {
            Arc::new(Self {
                source,
                answer: Err(error),
            })
        }
    }

    impl MetadataProvider for Fake {
        fn source(&self) -> MetadataSource {
            self.source
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities {
                identity: MediaSupport::Series,
                structure: MediaSupport::Series,
            }
        }

        fn credentials(&self) -> &'static [CredentialField] {
            &[]
        }

        fn verify(&self) -> MetaFuture<'_, Result<(), MetadataError>> {
            Box::pin(async { Ok(()) })
        }

        fn find(
            &self,
            _known: &ExternalId,
            _media: MediaType,
        ) -> MetaFuture<'_, Result<Option<ExternalId>, MetadataError>> {
            Box::pin(async { Ok(None) })
        }

        fn variants(
            &self,
            _series: &ExternalId,
        ) -> MetaFuture<'_, Result<Vec<StructureVariant>, MetadataError>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn tree(
            &self,
            _series: &ExternalId,
            _ordering: &Ordering,
        ) -> MetaFuture<'_, Result<SeriesTree, MetadataError>> {
            let answer = match &self.answer {
                Ok(tree) => Ok(tree.clone()),
                Err(e) => Err(clone_error(e)),
            };
            Box::pin(async move { answer })
        }
    }

    /// `MetadataError` is not `Clone` — it carries an `ExternalIdError` —
    /// and the fake has to answer more than once.
    fn clone_error(e: &MetadataError) -> MetadataError {
        match e {
            MetadataError::NotFound { origin, media, id } => MetadataError::NotFound {
                origin: *origin,
                media: *media,
                id: id.clone(),
            },
            MetadataError::Unavailable { origin, detail } => MetadataError::Unavailable {
                origin: *origin,
                detail: detail.clone(),
            },
            other => MetadataError::Malformed {
                origin: other.origin().unwrap_or(MetadataSource::Tmdb),
                detail: other.to_string(),
            },
        }
    }

    fn stored(source: MetadataSource, value: &str) -> StoredId {
        StoredId {
            id: ExternalId::new(source, value).unwrap(),
            verification: crate::db::item_ids::Verification::Asserted,
        }
    }

    /// **Every source is either a structure owner or explicitly not
    /// one.** Derived from an exhaustive `match`, so this walks the enum
    /// rather than a hand-written list — the difference between a guard
    /// that promises and one that catches.
    #[test]
    fn every_source_has_a_place_in_the_structure_order() {
        let ranked: Vec<_> = MetadataSource::all()
            .filter_map(|s| structure_rank(s).map(|r| (r, s)))
            .collect();

        // A source that can build a tree has to be somewhere in the
        // order, or it is configured and never asked.
        for source in MetadataSource::all() {
            let builds_trees = crate::metadata::registry::capabilities_of(source)
                .is_some_and(|c| c.structure.covers(MediaType::Tv));
            assert_eq!(
                builds_trees,
                structure_rank(source).is_some(),
                "{source} can build a tree but has no place in the order, or the reverse"
            );
        }

        let mut by_rank = ranked.clone();
        by_rank.sort_by_key(|(rank, _)| *rank);
        assert_eq!(
            by_rank.first().map(|(_, s)| *s),
            Some(MetadataSource::Tvdb),
            "the scene numbers releases the way TheTVDB does; it is asked first"
        );
    }

    /// The whole point: a series with no recorded owner is born under
    /// the source the scene follows, not under the one that used to be
    /// the only option.
    #[tokio::test]
    async fn a_new_series_is_born_under_the_source_the_scene_follows() {
        let registry = Registry::from_providers(vec![
            Fake::answering(MetadataSource::Tmdb),
            Fake::answering(MetadataSource::Tvdb),
        ]);
        let ids = [
            stored(MetadataSource::Tmdb, "1396"),
            stored(MetadataSource::Tvdb, "81189"),
        ];

        let tree = born(&registry, &ids).await.unwrap();
        assert_eq!(tree.source, MetadataSource::Tvdb);
    }

    /// A source that answered and does not hold the title hands it on.
    /// Half of brarr's catalogue is only on TMDB.
    #[tokio::test]
    async fn a_source_that_does_not_have_the_title_hands_it_to_the_next() {
        let registry = Registry::from_providers(vec![
            Fake::refusing(
                MetadataSource::Tvdb,
                MetadataError::NotFound {
                    origin: MetadataSource::Tvdb,
                    media: MediaType::Tv,
                    id: "81189".to_owned(),
                },
            ),
            Fake::answering(MetadataSource::Tmdb),
        ]);
        let ids = [
            stored(MetadataSource::Tmdb, "1396"),
            stored(MetadataSource::Tvdb, "81189"),
        ];

        let tree = born(&registry, &ids).await.unwrap();
        assert_eq!(tree.source, MetadataSource::Tmdb);
    }

    /// **The one that matters.** A source that could not be *asked* must
    /// not hand the title on: the owner is written at birth, so a
    /// timeout would decide the numbering of a series for good, and the
    /// symptom is a library that looks complete and searches under
    /// coordinates no release uses.
    #[tokio::test]
    async fn a_source_that_could_not_be_asked_does_not_hand_the_title_on() {
        let registry = Registry::from_providers(vec![
            Fake::refusing(
                MetadataSource::Tvdb,
                MetadataError::Unavailable {
                    origin: MetadataSource::Tvdb,
                    detail: "timeout".to_owned(),
                },
            ),
            Fake::answering(MetadataSource::Tmdb),
        ]);
        let ids = [
            stored(MetadataSource::Tmdb, "1396"),
            stored(MetadataSource::Tvdb, "81189"),
        ];

        let err = born(&registry, &ids).await.unwrap_err();
        assert!(
            matches!(
                &err,
                AppError::Metadata(MetadataError::Unavailable {
                    origin: MetadataSource::Tvdb,
                    ..
                })
            ),
            "expected the failure to propagate, got {err:?}"
        );
    }

    /// No id for a source is not a failure to report — there is no
    /// request to make — so it skips silently to the next candidate.
    #[tokio::test]
    async fn a_source_brarr_holds_no_id_for_is_skipped() {
        let registry = Registry::from_providers(vec![
            Fake::answering(MetadataSource::Tvdb),
            Fake::answering(MetadataSource::Tmdb),
        ]);
        let ids = [stored(MetadataSource::Tmdb, "1396")];

        let tree = born(&registry, &ids).await.unwrap();
        assert_eq!(tree.source, MetadataSource::Tmdb);
    }

    /// A deployment with no metadata credential at all says so, rather
    /// than reporting that the sources do not have the title.
    #[tokio::test]
    async fn an_unconfigured_deployment_names_the_credential() {
        let registry = Registry::from_providers(Vec::new());
        let err = born(&registry, &[stored(MetadataSource::Tmdb, "1396")])
            .await
            .unwrap_err();
        assert!(
            matches!(&err, AppError::InvalidInput(m) if m.contains("/settings")),
            "expected a named refusal, got {err:?}"
        );
    }

    /// A recorded owner is never re-decided. This is what stops the
    /// passive \*arr sweep — which rebuilds every series' tree every
    /// half hour — from walking a title over to a different provider
    /// the day a credential is added.
    #[tokio::test]
    async fn a_recorded_owner_is_not_re_decided() {
        let pool = open_memory().await.unwrap();
        let item = crate::db::library::upsert(&pool, &Seed::series(1_396, "Breaking Bad").build())
            .await
            .unwrap();
        crate::db::library::set_structure_choice(
            &pool,
            item.id,
            MetadataSource::Tmdb,
            &Ordering::Default,
            None,
            false,
        )
        .await
        .unwrap();
        crate::db::item_ids::put(
            &pool,
            item.id,
            MediaType::Tv,
            &ExternalId::new(MetadataSource::Tvdb, "81189").unwrap(),
            crate::db::item_ids::Verification::Asserted,
        )
        .await
        .unwrap();

        let registry = Registry::from_providers(vec![
            Fake::answering(MetadataSource::Tvdb),
            Fake::answering(MetadataSource::Tmdb),
        ]);

        let got = tree(&pool, &registry, item.id).await.unwrap();
        assert_eq!(
            got.source,
            MetadataSource::Tmdb,
            "the recorded owner decides, even with a preferred source configured"
        );
    }

    /// A title whose owner it carries no id for is refused by name,
    /// rather than fetched under whatever id happens to be there.
    #[tokio::test]
    async fn a_title_with_no_id_for_its_owner_is_refused() {
        let pool = open_memory().await.unwrap();
        let item = crate::db::library::upsert(&pool, &Seed::series(1_396, "Breaking Bad").build())
            .await
            .unwrap();
        // Claim it for TheTVDB without giving it a TheTVDB id.
        crate::db::library::set_structure_choice(
            &pool,
            item.id,
            MetadataSource::Tvdb,
            &Ordering::Default,
            None,
            true,
        )
        .await
        .unwrap();
        let registry = Registry::build(&pool).await.unwrap();

        let err = tree(&pool, &registry, item.id).await.unwrap_err();
        assert!(
            matches!(&err, AppError::InvalidInput(m) if m.contains("TheTVDB")),
            "expected a named refusal, got {err:?}"
        );
    }
}
