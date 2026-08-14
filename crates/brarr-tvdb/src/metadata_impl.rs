#![allow(
    clippy::doc_markdown,
    reason = "TheTVDB/TMDB acronyms appear too often in these docs to be worth backticking each instance"
)]

//! [`MetadataProvider`] for [`TvdbClient`].
//!
//! Follows the `brarr-tracker-unit3d` precedent: the trait lives in
//! `brarr-core`, the implementation lives beside the client that answers
//! it, and the orchestrator depends on neither directly.
//!
//! **This is what turns TheTVDB from a numbering source into a structure
//! owner.** The crate docs above still describe the older arrangement,
//! where the data landed in a translation table beside a TMDB-built tree;
//! what this impl offers instead is the tree itself, in the coordinates
//! that get stored.

use brarr_core::{
    Capabilities, CredentialField, ExternalId, MediaSupport, MediaType, MetaFuture, MetadataError,
    MetadataProvider, MetadataSource, Ordering, OrderingFamily, SeriesTree, StructureVariant,
    TreeEpisode, TreeSeason,
};

use crate::client::TvdbClient;
use crate::error::TvdbError;
use crate::model::{Episode, SeasonType};

/// What `/settings` renders for TheTVDB.
///
/// The PIN is **not** required, and that is load-bearing: it exists only
/// for a user-supported key and the API documentation says to remove it
/// entirely otherwise. Sending it empty is a refused login.
const CREDENTIALS: &[CredentialField] = &[
    CredentialField {
        key: "tvdb_api_key",
        label: "Chave de API da TheTVDB",
        secret: true,
        required: true,
    },
    CredentialField {
        key: "tvdb_pin",
        label: "PIN de assinante (só para chave user-supported)",
        secret: true,
        required: false,
    },
];

impl MetadataProvider for TvdbClient {
    fn source(&self) -> MetadataSource {
        MetadataSource::Tvdb
    }

    /// Series only, on both axes.
    ///
    /// TheTVDB has films, and this client does not read them: the
    /// endpoints it speaks are the series ones, and a `Both` here would
    /// be a claim the registry acts on and the code cannot meet. Declared
    /// narrow rather than aspirational — that is what keeps
    /// [`MetadataError::Unsupported`] a bug report.
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            identity: MediaSupport::Series,
            structure: MediaSupport::Series,
        }
    }

    fn credentials(&self) -> &'static [CredentialField] {
        CREDENTIALS
    }

    fn verify(&self) -> MetaFuture<'_, Result<(), MetadataError>> {
        Box::pin(async move { Self::verify(self).await.map_err(translate) })
    }

    fn find(
        &self,
        known: &ExternalId,
        media: MediaType,
    ) -> MetaFuture<'_, Result<Option<ExternalId>, MetadataError>> {
        let remote = known.value().to_owned();
        Box::pin(async move {
            if media != MediaType::Tv {
                return Err(MetadataError::Unsupported {
                    origin: MetadataSource::Tvdb,
                    capability: "find",
                    media,
                });
            }
            let found = self.series_by_remote_id(&remote).await.map_err(translate)?;
            found
                .map(|id| ExternalId::new(MetadataSource::Tvdb, &id.to_string()))
                .transpose()
                .map_err(MetadataError::BadId)
        })
    }

    /// The season types that renumber, and only those.
    ///
    /// `Default` and `Official` are left out on purpose: `Official` *is*
    /// what [`Self::tree`] builds when asked for [`Ordering::Default`],
    /// so listing it would offer the tree that is already there.
    ///
    /// Answered without a network call. Probing each type to see which
    /// ones a series really has would be four requests per title against
    /// somebody else's free tier, on a screen the operator opens to look
    /// rather than to choose; a type a series does not have comes back
    /// empty from [`Self::tree`], which is where the refusal belongs.
    fn variants(
        &self,
        _series: &ExternalId,
    ) -> MetaFuture<'_, Result<Vec<StructureVariant>, MetadataError>> {
        Box::pin(async move {
            Ok(vec![
                variant(OrderingFamily::Dvd, SeasonType::Dvd, "Ordem do DVD"),
                variant(
                    OrderingFamily::Absolute,
                    SeasonType::Absolute,
                    "Numeração absoluta",
                ),
                variant(
                    OrderingFamily::Alternate,
                    SeasonType::Alternate,
                    "Ordenação alternativa",
                ),
            ])
        })
    }

    fn tree(
        &self,
        series: &ExternalId,
        ordering: &Ordering,
    ) -> MetaFuture<'_, Result<SeriesTree, MetadataError>> {
        let id = series.clone();
        let ordering = ordering.clone();
        Box::pin(async move {
            let numeric = i64::from(id.as_u32().map_err(MetadataError::BadId)?);
            let season_type = match &ordering {
                // `Official` rather than `Default`: the broadcast split is
                // the one the scene follows, and `Default` is whatever a
                // contributor set the series to.
                Ordering::Default | Ordering::Manual { .. } => SeasonType::Official,
                Ordering::Named { handle, .. } => {
                    season_type_of(handle).ok_or_else(|| MetadataError::UnknownOrdering {
                        origin: MetadataSource::Tvdb,
                        id: id.value().to_owned(),
                        handle: handle.to_string(),
                    })?
                }
            };

            let found = self
                .series_episodes(numeric, season_type, None)
                .await
                .map_err(translate)?;
            if found.episodes.is_empty() {
                return Err(MetadataError::Empty {
                    origin: MetadataSource::Tvdb,
                    id: id.value().to_owned(),
                });
            }

            let tree = SeriesTree {
                source: MetadataSource::Tvdb,
                ordering: ordering.clone(),
                seasons: seasons_of(&found.episodes),
            };
            match &ordering {
                // Blocks are applied on top of the broadcast split by the
                // shared transformation in `brarr-core`, so what this
                // returns is still the coordinates that get stored.
                Ordering::Manual { blocks } => {
                    tree.recut(blocks).map_err(|e| MetadataError::Malformed {
                        origin: MetadataSource::Tvdb,
                        detail: e.to_string(),
                    })
                }
                Ordering::Default | Ordering::Named { .. } => Ok(tree),
            }
        })
    }
}

/// One offered ordering, with the season-type segment as its handle.
fn variant(family: OrderingFamily, season_type: SeasonType, name: &str) -> StructureVariant {
    StructureVariant {
        family,
        handle: season_type.as_str().to_owned(),
        name: name.to_owned(),
        // TheTVDB does not report how much of a series an alternate
        // ordering covers, and `None` says that rather than implying
        // full coverage with a guessed pair.
        coverage: None,
    }
}

/// The season type a stored handle names.
fn season_type_of(handle: &str) -> Option<SeasonType> {
    [
        SeasonType::Default,
        SeasonType::Official,
        SeasonType::Dvd,
        SeasonType::Absolute,
        SeasonType::Alternate,
        SeasonType::Regional,
    ]
    .into_iter()
    .find(|t| t.as_str() == handle)
}

/// Group a flat episode list into seasons, preserving the order the API
/// returned them in within each season.
fn seasons_of(episodes: &[Episode]) -> Vec<TreeSeason> {
    let mut numbers: Vec<i32> = episodes.iter().map(|e| e.season_number).collect();
    numbers.sort_unstable();
    numbers.dedup();

    numbers
        .into_iter()
        .map(|number| {
            let mut held: Vec<&Episode> = episodes
                .iter()
                .filter(|e| e.season_number == number)
                .collect();
            held.sort_by_key(|e| e.number);
            TreeSeason {
                number,
                // TheTVDB's episode endpoint carries no season air date,
                // so it is derived from the earliest episode that has
                // one rather than left blank.
                air_date: held.iter().filter_map(|e| e.aired).min(),
                episodes: held
                    .iter()
                    .map(|e| TreeEpisode {
                        external_id: e.id.to_string(),
                        number: e.number,
                        title: e.name.clone(),
                        air_date: e.aired,
                        absolute_number: e.absolute_number,
                    })
                    .collect(),
            }
        })
        .collect()
}

/// Peel this crate's error into the neutral one.
///
/// The two failures worth naming keep their identity: a refused
/// credential is the operator's to fix, and an unknown id is not a
/// failure at all in most call paths.
fn translate(e: TvdbError) -> MetadataError {
    let origin = MetadataSource::Tvdb;
    match e {
        TvdbError::Unauthorized | TvdbError::TokenRejected => {
            MetadataError::Unauthorized { origin }
        }
        TvdbError::NotFound(what) => MetadataError::NotFound {
            origin,
            media: MediaType::Tv,
            id: what,
        },
        TvdbError::RateLimited | TvdbError::Http(_) => MetadataError::Unavailable {
            origin,
            detail: e.to_string(),
        },
        TvdbError::BadJson(_)
        | TvdbError::BadUrl(_)
        | TvdbError::ClientBuild(_)
        | TvdbError::RunawayPagination(_) => MetadataError::Malformed {
            origin,
            detail: e.to_string(),
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn ep(id: i64, season: i32, number: i32, absolute: Option<i32>) -> Episode {
        Episode {
            id,
            name: Some(format!("E{number}")),
            season_number: season,
            number,
            absolute_number: absolute,
            aired: Some(datetime!(2015-10-18 0:00 UTC)),
        }
    }

    /// **The grouping that makes TheTVDB a structure owner.** A flat
    /// episode list becomes the five-season shape the disk uses.
    #[test]
    fn a_flat_list_becomes_the_broadcast_shape() {
        let mut episodes = Vec::new();
        for (season, count) in [(0, 2), (1, 14), (2, 13)] {
            for number in 1..=count {
                episodes.push(ep(i64::from(season * 100 + number), season, number, None));
            }
        }
        // Out of order on the wire, which the API does not promise
        // anything about.
        episodes.reverse();

        let seasons = seasons_of(&episodes);
        assert_eq!(
            seasons.iter().map(|s| s.number).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(seasons[1].episodes.len(), 14);
        assert_eq!(seasons[1].episodes[0].number, 1, "sorted within a season");
        assert_eq!(seasons[1].episodes[13].number, 14);
        assert!(seasons[1].air_date.is_some(), "derived from its episodes");
    }

    /// The id is carried as text and never invented: it is the only
    /// identity that survives a change of ordering.
    #[test]
    fn every_episode_keeps_its_own_id() {
        let seasons = seasons_of(&[ep(5_345_648, 2, 1, Some(15))]);
        let episode = &seasons[0].episodes[0];
        assert_eq!(episode.external_id, "5345648");
        assert_eq!(episode.absolute_number, Some(15));
    }

    /// Handles round-trip, or a stored ordering stops resolving after a
    /// restart.
    #[tokio::test]
    async fn every_offered_variant_resolves_back_to_a_season_type() {
        let client = TvdbClient::new(crate::TvdbAuth {
            api_key: "k".to_owned(),
            pin: None,
        })
        .unwrap();
        let offered = client
            .variants(&ExternalId::new(MetadataSource::Tvdb, "295068").unwrap())
            .await
            .unwrap();

        assert!(!offered.is_empty());
        for variant in &offered {
            assert!(
                season_type_of(&variant.handle).is_some(),
                "{} does not resolve",
                variant.handle
            );
            assert!(
                variant.family.renumbers(),
                "{:?} does not renumber, so offering it offers what is already there",
                variant.family
            );
            assert_eq!(variant.ordering().handle(), Some(variant.handle.as_str()));
        }
        // `Official` is what `tree` builds for `Ordering::Default`, so
        // offering it would be offering the tree that is already there.
        assert!(
            !offered.iter().any(|v| v.handle == "official"),
            "the default ordering must not be offered as a choice"
        );
    }

    /// Capabilities are honest about what this client reads.
    #[test]
    fn capabilities_are_coherent_and_narrow() {
        let client = TvdbClient::new(crate::TvdbAuth {
            api_key: "k".to_owned(),
            pin: None,
        })
        .unwrap();
        let caps = MetadataProvider::capabilities(&client);
        assert!(caps.is_coherent());
        assert!(caps.structure.covers(MediaType::Tv));
        assert!(!caps.identity.covers(MediaType::Movie));
    }

    /// The PIN must be optional, or a project key cannot be configured
    /// without inventing a value the API refuses.
    #[test]
    fn the_pin_is_not_required() {
        let pin = CREDENTIALS.iter().find(|c| c.key == "tvdb_pin").unwrap();
        assert!(!pin.required);
        assert!(pin.secret);
        let key = CREDENTIALS
            .iter()
            .find(|c| c.key == "tvdb_api_key")
            .unwrap();
        assert!(key.required);
    }

    /// A refused key and an unreachable server must not become one
    /// error, or a sweep retries what a person has to fix.
    #[test]
    fn the_two_failures_worth_naming_keep_their_identity() {
        assert!(matches!(
            translate(TvdbError::Unauthorized),
            MetadataError::Unauthorized { .. }
        ));
        assert!(matches!(
            translate(TvdbError::TokenRejected),
            MetadataError::Unauthorized { .. }
        ));
        assert!(matches!(
            translate(TvdbError::NotFound("series 1".to_owned())),
            MetadataError::NotFound { .. }
        ));
        assert!(translate(TvdbError::RateLimited).is_transient());
        assert!(!translate(TvdbError::Unauthorized).is_transient());
    }
}
