//! Turning what brarr knows a title *is* into what a tracker can be
//! asked.
//!
//! Two different questions that were one before this module. A catalogue
//! identity is `library_item_ids` — text, canonicalised, one row per
//! source. A search axis is [`TmdbId`] / [`ImdbId`] / [`TvdbId`] —
//! non-zero `u32`s the fan-out understands. Every id is the first; only
//! some are usable as the second.
//!
//! ## Why the rejections are returned
//!
//! `movie_target` built the axis with
//! `u32::try_from(..).ok().and_then(TmdbId::new)` and its caller wrote
//! `movie_target(item).into_iter().collect()`. So a film whose id will
//! not convert leaves the sweep with **no counter and no log** — the
//! title renders "faltando" forever and the screen blames the trackers,
//! which is the costly direction of the lie.
//!
//! [`resolve`] hands back both halves. Nothing is dropped; a caller that
//! wants to ignore the rejections has to say so.

use brarr_core::{ExternalId, ExternalIdError, ImdbId, MediaType, MetadataSource, TmdbId, TvdbId};

use crate::db::item_ids::StoredId;

/// The ids the tracker fan-out can actually use.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SearchAxis {
    /// UNIT3D's axis.
    pub tmdb: Option<TmdbId>,
    /// Newznab movie-search's axis.
    pub imdb: Option<ImdbId>,
    /// Newznab tv-search's axis, and the only one for an episode.
    pub tvdb: Option<TvdbId>,
}

impl SearchAxis {
    /// Whether anything can be searched with this at all.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.tmdb.is_none() && self.imdb.is_none() && self.tvdb.is_none()
    }

    /// Whether an episode can be searched for.
    ///
    /// Its own question because the answer differs from [`Self::is_empty`]
    /// in a way that matters: TVDB is the only per-episode axis any
    /// indexer speaks, so a series with a TMDB id and nothing else is
    /// searchable as a *title* and not as an episode.
    #[must_use]
    pub const fn can_search_episodes(&self) -> bool {
        self.tvdb.is_some()
    }
}

/// An id brarr holds but cannot search with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AxisRejection {
    /// Who issued the id.
    pub source: MetadataSource,
    /// The stored value, as it is.
    pub value: String,
    /// Why it will not convert.
    pub reason: Reason,
}

impl AxisRejection {
    /// One sentence for the screen, naming the cause.
    ///
    /// The badge used to say "nada encontrado" for this and for a genuine
    /// miss, which sends the operator to look at their trackers for a
    /// problem that is in their catalogue.
    #[must_use]
    pub fn message(&self) -> String {
        match self.reason {
            Reason::OutOfRange => format!(
                "o id {} `{}` está fora da faixa que os indexadores aceitam",
                self.source, self.value
            ),
            Reason::NotIndexed => format!(
                "nenhum indexador busca por id da {} — resolva um id do TMDB ou da TheTVDB",
                self.source
            ),
        }
    }
}

/// Why an id cannot become a search key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// Numerically unusable: zero, negative, or past `u32`.
    OutOfRange,
    /// A perfectly good id in a namespace no tracker indexes by.
    ///
    /// A separate reason because the fix is different, and because it is
    /// the one a third provider makes common: an AniList id is not
    /// broken, it is simply not something an indexer accepts.
    NotIndexed,
}

/// Resolve a catalogue identity into a search axis, keeping what it
/// could not use.
///
/// `media` is taken because the axes are not interchangeable across
/// kinds — TVDB is series-only upstream, and carrying one on a film would
/// produce a query no indexer answers.
#[must_use]
pub fn resolve(ids: &[StoredId], media: MediaType) -> (SearchAxis, Vec<AxisRejection>) {
    let mut axis = SearchAxis::default();
    let mut rejected = Vec::new();

    for stored in ids {
        let id = &stored.id;
        match id.source() {
            MetadataSource::Tmdb => match narrow(id, TmdbId::new) {
                Ok(value) => axis.tmdb = Some(value),
                Err(reason) => rejected.push(reject(id, reason)),
            },
            MetadataSource::Imdb => match narrow(id, ImdbId::new) {
                Ok(value) => axis.imdb = Some(value),
                Err(reason) => rejected.push(reject(id, reason)),
            },
            MetadataSource::Tvdb => {
                if media == MediaType::Movie {
                    // Not a rejection: TMDB does not even publish a TVDB
                    // id for a film, so one being present is harmless
                    // extra knowledge rather than a problem to report.
                    continue;
                }
                match narrow(id, TvdbId::new) {
                    Ok(value) => axis.tvdb = Some(value),
                    Err(reason) => rejected.push(reject(id, reason)),
                }
            }
        }
    }

    (axis, rejected)
}

/// Narrow a canonical id into one of the fan-out's newtypes.
fn narrow<T, E>(id: &ExternalId, build: impl Fn(u32) -> Result<T, E>) -> Result<T, Reason> {
    let numeric = id.as_u32().map_err(|e| match e {
        ExternalIdError::Empty { .. } | ExternalIdError::Malformed { .. } => Reason::OutOfRange,
    })?;
    build(numeric).map_err(|_| Reason::OutOfRange)
}

fn reject(id: &ExternalId, reason: Reason) -> AxisRejection {
    AxisRejection {
        source: id.source(),
        value: id.value().to_owned(),
        reason,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;
    use crate::db::item_ids::Verification;

    fn stored(source: MetadataSource, raw: &str) -> StoredId {
        StoredId {
            id: ExternalId::new(source, raw).unwrap(),
            verification: Verification::Asserted,
        }
    }

    #[test]
    fn a_full_identity_becomes_a_full_axis() {
        let ids = vec![
            stored(MetadataSource::Tmdb, "76479"),
            stored(MetadataSource::Imdb, "tt1190634"),
            stored(MetadataSource::Tvdb, "355567"),
        ];
        let (axis, rejected) = resolve(&ids, MediaType::Tv);
        assert!(rejected.is_empty());
        assert_eq!(axis.tmdb.map(TmdbId::get), Some(76_479));
        assert_eq!(axis.imdb.map(ImdbId::get), Some(1_190_634));
        assert_eq!(axis.tvdb.map(TvdbId::get), Some(355_567));
        assert!(axis.can_search_episodes());
    }

    /// **The hole this module exists to close.** A title whose only usable
    /// axis is missing is reported, not dropped — the old path lost the
    /// film with no counter and no log, and the screen blamed the
    /// trackers.
    #[test]
    fn an_unusable_id_is_counted_not_dropped() {
        // Past `u32`: a value a hand-edited row or a bad import can hold.
        let ids = vec![stored(MetadataSource::Tmdb, "99999999999")];
        let (axis, rejected) = resolve(&ids, MediaType::Movie);

        assert!(axis.is_empty());
        assert_eq!(rejected.len(), 1);
        assert_eq!(rejected[0].source, MetadataSource::Tmdb);
        assert_eq!(rejected[0].reason, Reason::OutOfRange);
        // And the sentence names the catalogue, not the trackers.
        assert!(rejected[0].message().contains("indexadores"));
        assert!(rejected[0].message().contains("99999999999"));
    }

    /// A series with a TMDB id and no TVDB one is searchable as a title
    /// and not as an episode. Collapsing the two answers is what made the
    /// sweep badge blame the trackers for a catalogue gap.
    #[test]
    fn a_series_without_a_tvdb_id_cannot_search_episodes() {
        let ids = vec![stored(MetadataSource::Tmdb, "95479")];
        let (axis, rejected) = resolve(&ids, MediaType::Tv);
        assert!(rejected.is_empty(), "nothing is wrong with the id it has");
        assert!(!axis.is_empty(), "the title is still searchable");
        assert!(!axis.can_search_episodes());
    }

    /// A TVDB id on a film is not a problem to report: TMDB does not
    /// publish one for a movie, so its presence is extra knowledge.
    #[test]
    fn a_tvdb_id_on_a_film_is_ignored_quietly() {
        let ids = vec![
            stored(MetadataSource::Tmdb, "603"),
            stored(MetadataSource::Tvdb, "1"),
        ];
        let (axis, rejected) = resolve(&ids, MediaType::Movie);
        assert!(rejected.is_empty());
        assert_eq!(axis.tvdb, None);
        assert_eq!(axis.tmdb.map(TmdbId::get), Some(603));
    }

    /// Every source resolves to something — an axis or a named reason —
    /// so a provider added without an axis rule cannot silently produce
    /// a title nothing ever searches for.
    #[test]
    fn every_source_is_either_an_axis_or_a_named_rejection() {
        for source in MetadataSource::all() {
            let ids = vec![stored(source, "1")];
            let (axis, rejected) = resolve(&ids, MediaType::Tv);
            assert!(
                !axis.is_empty() || !rejected.is_empty(),
                "{source} produced neither an axis nor a rejection"
            );
        }
    }

    #[test]
    fn an_empty_identity_is_an_empty_axis() {
        let (axis, rejected) = resolve(&[], MediaType::Tv);
        assert!(axis.is_empty());
        assert!(rejected.is_empty());
    }
}
