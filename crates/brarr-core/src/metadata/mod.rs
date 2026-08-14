//! [`MetadataProvider`] — the abstraction over "something that knows who
//! a title is, and how its episodes are numbered".
//!
//! Two implementations, which is what justifies the trait:
//! `brarr_tmdb::TmdbClient` and `brarr_tvdb::TvdbClient`. Both honour
//! every method here; there is none that one of them answers with
//! [`MetadataError::Unsupported`].
//!
//! # The disagreement this exists to settle
//!
//! The two providers do not agree about where a season ends. Dragon Ball
//! Super is one season of 131 on TMDB and 14/13/19/30/55 on TheTVDB, and
//! the scene follows the second. Jujutsu Kaisen is 1×59 on TMDB and
//! `S02E23` in every release.
//!
//! The previous answer was a translation table read at the search axis,
//! at the file pairing and at the import destination. It did not
//! converge — each of those three learned about the translation
//! separately, and every repair uncovered the next defect. This trait is
//! where the disagreement becomes a **choice recorded on the item**
//! instead: [`MetadataProvider::tree`] returns the coordinates that will
//! be stored, and nothing translates them afterwards.
//!
//! # Description arrived the day it had two implementations
//!
//! [`MetadataProvider::describe`] was deliberately absent while
//! TheTVDB's *episode* endpoint was all this crate read of it — a series
//! id and a name, no overview, no status, no runtime, no poster. A trait
//! with one implementation is what this workspace forbids, and its
//! escape hatches (`unimplemented!`, or `Err` on every call) are
//! respectively banned and dishonest.
//!
//! `/series/{id}/extended` and `/series/{id}/translations/{lang}` are the
//! second implementation, so the method is here. It is also why
//! [`Artwork`] carries its source: TMDB stores a path relative to its
//! CDN and TheTVDB returns an absolute URL, so the same string means two
//! things and cannot become a URL without knowing who wrote it.
//!
//! # Capabilities are consulted before dispatch, not after
//!
//! [`MetadataError::Unsupported`] exists so "I do not do this" is
//! distinguishable from "I found nothing" — the defect the sibling
//! [`TrackerProvider`](crate::TrackerProvider) carries today, where
//! `search_by_tvdb` defaults to `Ok(vec![])` and every WASM plugin
//! therefore reports a healthy zero for every series search. The
//! registry filters on [`MetadataProvider::capabilities`] first, so that
//! variant should only ever fire on a bug — and when it does, it fires
//! loudly.

use std::future::Future;
use std::pin::Pin;

mod capability;
mod description;
mod ids;
mod tree;

pub use capability::{Capabilities, CredentialField, MediaSupport};
pub use description::{Artwork, Description, ProductionStatus};
pub use ids::{ExternalId, ExternalIdError, MediaType, MetadataSource, SourceKind};
pub use tree::{
    Block, BlockError, Ordering, OrderingFamily, SeriesTree, StructureVariant, TreeEpisode,
    TreeSeason,
};

/// Heap-allocated boxed future, matching
/// [`ProviderFuture`](crate::ProviderFuture).
///
/// Native `async fn` in trait is stable, but the resulting trait is not
/// `dyn`-compatible without explicit boxing, and the registry holds
/// providers chosen at runtime from a column.
pub type MetaFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A source of media identity and of series structure.
pub trait MetadataProvider: Send + Sync {
    /// Which source this is. Written into every `source` column, and
    /// backed by a row in `metadata_sources`.
    fn source(&self) -> MetadataSource;

    /// What this provider answers for. Consulted by the registry before
    /// every dispatch.
    fn capabilities(&self) -> Capabilities;

    /// Credential fields `/settings` should render for this provider.
    fn credentials(&self) -> &'static [CredentialField];

    /// Prove the credential works, with no side effect.
    ///
    /// # Errors
    ///
    /// [`MetadataError::Unauthorized`] when the credential is missing or
    /// refused — its own variant because the operator fixes that one and
    /// merely waits out the others. A `200 OK` carrying no usable payload
    /// counts as refusal: trusting the status code alone reports a broken
    /// key as a healthy connection, which is why
    /// `DownloadClientError::Auth` exists on the other side of this
    /// workspace.
    fn verify(&self) -> MetaFuture<'_, Result<(), MetadataError>>;

    /// Cross-resolution: "what id do you have for this id of another
    /// source?"
    ///
    /// `Ok(None)` means the provider answered and does not know the
    /// title — a fact worth recording, because the alternative is asking
    /// again on every sweep.
    ///
    /// # Errors
    ///
    /// [`MetadataError`] per the variant docs.
    fn find(
        &self,
        known: &ExternalId,
        media: MediaType,
    ) -> MetaFuture<'_, Result<Option<ExternalId>, MetadataError>>;

    /// The orderings this provider offers for a series, so the picker
    /// shows a choice instead of a default.
    ///
    /// An empty vec is a legal answer and means "one ordering, no
    /// variants" — which is most series.
    ///
    /// # Errors
    ///
    /// [`MetadataError`] per the variant docs.
    fn variants(
        &self,
        series: &ExternalId,
    ) -> MetaFuture<'_, Result<Vec<StructureVariant>, MetadataError>>;

    /// One title as this provider describes it.
    ///
    /// **A single owner per item**, unlike identity, which is a set:
    /// `title`, `year`, `original_title` and `overview` all describe one
    /// cut of one work, so taking the title from one provider and the
    /// year from another produces a record that describes nothing.
    ///
    /// Unlike [`Self::tree`], a policy change may overwrite a recorded
    /// descriptive owner without asking. Rewriting a synopsis is cheap
    /// and reversible; rebuilding a tree re-points every acquisition
    /// hanging off the item, which is why that one is a choice and this
    /// one is not.
    ///
    /// # Errors
    ///
    /// [`MetadataError::NotFound`] when the provider holds no record
    /// under this id, and the credential and transport variants. A
    /// provider that has the title but no translation of it answers with
    /// the fields it has and leaves the rest absent — saying nothing is
    /// better than saying it in a language nobody asked for.
    fn describe(
        &self,
        id: &ExternalId,
        media: MediaType,
    ) -> MetaFuture<'_, Result<Description, MetadataError>>;

    /// The tree, under `ordering`.
    ///
    /// **The contract is that the coordinates returned are the ones that
    /// will be persisted.**
    ///
    /// # Errors
    ///
    /// [`MetadataError::Empty`] when the call succeeded and carried no
    /// episodes. That is deliberately neither `Ok` with an empty tree nor
    /// [`MetadataError::NotFound`]: the translation table learned this
    /// the hard way, where "the two numberings agree" and "the provider
    /// answered with nothing" were the same value and the second quietly
    /// erased a valid translation. An empty tree written over a live one
    /// would orphan every acquisition on the item.
    fn tree(
        &self,
        series: &ExternalId,
        ordering: &Ordering,
    ) -> MetaFuture<'_, Result<SeriesTree, MetadataError>>;
}

/// Why a metadata call did not produce a usable answer.
///
/// English messages, like every other library crate here; the
/// orchestrator words them for the screen at its own boundary.
#[derive(Debug, thiserror::Error)]
pub enum MetadataError {
    /// Credential missing or refused, including a `200 OK` whose body
    /// says otherwise.
    #[error("{origin} credential missing or refused")]
    Unauthorized {
        /// Who refused.
        origin: MetadataSource,
    },

    /// The provider answered and holds no record under this id.
    #[error("{origin} does not know {media} {id}")]
    NotFound {
        /// Who was asked.
        origin: MetadataSource,
        /// What kind of title.
        media: MediaType,
        /// The id that found nothing.
        id: String,
    },

    /// The provider does not offer this ordering for this series.
    #[error("{origin} does not offer ordering `{handle}` for {id}")]
    UnknownOrdering {
        /// Who was asked.
        origin: MetadataSource,
        /// The series.
        id: String,
        /// The handle that did not resolve.
        handle: String,
    },

    /// A success that carried nothing usable. Distinct from
    /// [`Self::NotFound`] and [`Self::Unavailable`] on purpose — see
    /// [`MetadataProvider::tree`].
    #[error("{origin} returned no episodes for {id}")]
    Empty {
        /// Who answered.
        origin: MetadataSource,
        /// The series that came back empty.
        id: String,
    },

    /// Transport, timeout, rate limit. Transient.
    #[error("{origin} unreachable: {detail}")]
    Unavailable {
        /// Who could not be reached.
        origin: MetadataSource,
        /// What went wrong.
        detail: String,
    },

    /// A response that could not be read.
    #[error("{origin} response could not be read: {detail}")]
    Malformed {
        /// Who answered.
        origin: MetadataSource,
        /// What went wrong.
        detail: String,
    },

    /// An id that is not the shape its source uses.
    #[error("{0}")]
    BadId(#[from] ExternalIdError),

    /// The caller dispatched outside [`MetadataProvider::capabilities`].
    ///
    /// Should be unreachable; when it is reached it is a bug, and it says
    /// so rather than returning an empty list that reads as success.
    #[error("{origin} does not serve {capability} for {media}")]
    Unsupported {
        /// Who was asked wrongly.
        origin: MetadataSource,
        /// Which method.
        capability: &'static str,
        /// For which media kind.
        media: MediaType,
    },
}

impl MetadataError {
    /// Who the failure is about.
    #[must_use]
    pub const fn origin(&self) -> Option<MetadataSource> {
        match self {
            Self::Unauthorized { origin }
            | Self::NotFound { origin, .. }
            | Self::UnknownOrdering { origin, .. }
            | Self::Empty { origin, .. }
            | Self::Unavailable { origin, .. }
            | Self::Malformed { origin, .. }
            | Self::Unsupported { origin, .. } => Some(*origin),
            Self::BadId(_) => None,
        }
    }

    /// Whether another attempt could succeed without anyone intervening.
    ///
    /// The split that matters to a sweep: a refused credential and an
    /// unknown id are the operator's to fix, and retrying them just
    /// hammers somebody else's API.
    #[must_use]
    pub const fn is_transient(&self) -> bool {
        matches!(self, Self::Unavailable { .. } | Self::Malformed { .. })
    }

    /// Whether the provider answered and simply does not cover this
    /// title.
    ///
    /// The split a **fallback** needs, and it is deliberately not the
    /// complement of [`Self::is_transient`]. Falling through to the next
    /// candidate is only sound when the current one *said no*: it did
    /// look, and it has nothing. A refused credential and a timeout mean
    /// nobody looked, and treating those as absence would let a key
    /// somebody forgot to paste decide who owns a series' shape — a
    /// decision recorded at birth and only the operator undoes it.
    ///
    /// Written as an exhaustive `match` rather than a `matches!`, so a
    /// new failure mode has to be classified by whoever adds it instead
    /// of inheriting "keep looking" from a wildcard.
    #[must_use]
    pub const fn is_absence(&self) -> bool {
        match self {
            Self::NotFound { .. } | Self::Empty { .. } => true,
            Self::Unauthorized { .. }
            | Self::UnknownOrdering { .. }
            | Self::Unavailable { .. }
            | Self::Malformed { .. }
            | Self::BadId(_)
            | Self::Unsupported { .. } => false,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    /// Compile-time check that the trait is `dyn`-compatible, which is
    /// what lets the registry hold providers picked at runtime.
    #[test]
    fn trait_is_dyn_compatible() {
        fn _accepts_dyn(_p: &dyn MetadataProvider) {}
    }

    /// **"I found nothing" and "I could not look" must not be one
    /// value.** Every variant is classified deliberately, so a new
    /// failure mode cannot inherit a default and become a silent retry
    /// loop against someone else's free tier.
    #[test]
    fn only_the_failures_that_heal_are_transient() {
        let transient = MetadataError::Unavailable {
            origin: MetadataSource::Tvdb,
            detail: "timeout".to_owned(),
        };
        assert!(transient.is_transient());

        for permanent in [
            MetadataError::Unauthorized {
                origin: MetadataSource::Tmdb,
            },
            MetadataError::NotFound {
                origin: MetadataSource::Tmdb,
                media: MediaType::Tv,
                id: "1".to_owned(),
            },
            MetadataError::Empty {
                origin: MetadataSource::Tvdb,
                id: "1".to_owned(),
            },
            MetadataError::Unsupported {
                origin: MetadataSource::Tvdb,
                capability: "tree",
                media: MediaType::Movie,
            },
        ] {
            assert!(!permanent.is_transient(), "{permanent}");
        }
    }

    /// **Only a "no" is an absence.** A fallback walks to the next
    /// source on this predicate, and the owner it lands on is written
    /// down, so a credential nobody pasted must not be able to answer
    /// the question "who owns this series' shape?".
    #[test]
    fn only_an_answered_no_counts_as_absence() {
        for absent in [
            MetadataError::NotFound {
                origin: MetadataSource::Tvdb,
                media: MediaType::Tv,
                id: "295068".to_owned(),
            },
            MetadataError::Empty {
                origin: MetadataSource::Tvdb,
                id: "295068".to_owned(),
            },
        ] {
            assert!(absent.is_absence(), "{absent}");
        }

        for asked_nothing in [
            MetadataError::Unauthorized {
                origin: MetadataSource::Tvdb,
            },
            MetadataError::Unavailable {
                origin: MetadataSource::Tvdb,
                detail: "timeout".to_owned(),
            },
            MetadataError::Malformed {
                origin: MetadataSource::Tvdb,
                detail: "bad json".to_owned(),
            },
            MetadataError::Unsupported {
                origin: MetadataSource::Tvdb,
                capability: "tree",
                media: MediaType::Movie,
            },
            MetadataError::BadId(ExternalIdError::Empty {
                origin: MetadataSource::Tvdb,
            }),
        ] {
            assert!(!asked_nothing.is_absence(), "{asked_nothing}");
        }
    }

    /// Every failure names who it is about, or the log says a call
    /// failed without saying whose.
    #[test]
    fn a_failure_names_its_source() {
        assert_eq!(
            MetadataError::Empty {
                origin: MetadataSource::Tvdb,
                id: "295068".to_owned(),
            }
            .origin(),
            Some(MetadataSource::Tvdb)
        );
        // The one that genuinely has no source: a malformed id was never
        // sent anywhere.
        assert_eq!(
            MetadataError::BadId(ExternalIdError::Empty {
                origin: MetadataSource::Tmdb
            })
            .origin(),
            None
        );
    }
}
