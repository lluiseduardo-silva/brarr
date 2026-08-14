//! Fetch the tree a title is actually under.
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
//! **An unclaimed title reads as TMDB under its own ordering**, which is
//! what every series in this catalogue was before the identity migration
//! and what a newly added one still is. That keeps the behaviour of the
//! ordinary path exactly as it was, and it is why this can replace the
//! old call without a flag.

use brarr_core::{MediaType, MetadataSource, SeriesTree};
use uuid::Uuid;

use crate::db::{Pool, item_ids};
use crate::error::AppError;
use crate::metadata::registry::Registry;
use crate::structure;

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
    let source = owner.source.unwrap_or(MetadataSource::Tmdb);
    let ordering = structure::ordering_of(&owner);

    let ids = item_ids::for_item(pool, item_id).await?;
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
        return Err(AppError::Metadata(brarr_core::MetadataError::Unsupported {
            origin: source,
            capability: "tree",
            media: MediaType::Tv,
        }));
    }

    Ok(provider.tree(&known.id, &ordering).await?)
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

    /// An unclaimed title asks TMDB, which is what every series was
    /// before the identity migration and what a new one still is.
    ///
    /// Asserted through the error rather than a network call: with no
    /// credential the registry has no TMDB provider, and `require`
    /// names the source it went looking for.
    #[tokio::test]
    async fn an_unclaimed_title_is_asked_of_tmdb() {
        let pool = open_memory().await.unwrap();
        let item = crate::db::library::upsert(&pool, &Seed::series(1_396, "Breaking Bad").build())
            .await
            .unwrap();
        let registry = Registry::build(&pool).await.unwrap();

        let err = tree(&pool, &registry, item.id).await.unwrap_err();
        assert!(
            err.to_string().contains("TMDB"),
            "an unclaimed title reads as TMDB: {err}"
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
            &brarr_core::Ordering::Default,
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
