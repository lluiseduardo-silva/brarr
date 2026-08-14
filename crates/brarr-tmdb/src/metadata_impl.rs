//! [`MetadataProvider`] for [`TmdbClient`].
//!
//! The second implementation, and the reason the trait is allowed to
//! exist. It is also the one that keeps the catalogue working the way it
//! does today: TMDB owns identity for films and series alike, and its
//! `/tv/{id}/season/{n}` walk is the tree every existing title is built
//! from.

use std::collections::HashMap;

use brarr_core::{
    Artwork, Capabilities, CredentialField, Description, ExternalId, MediaSupport, MediaType,
    MetaFuture, MetadataError, MetadataProvider, MetadataSource, Ordering, OrderingFamily,
    ProductionStatus, SeriesTree, StructureVariant, TreeEpisode, TreeSeason,
};
use time::{Date, OffsetDateTime, Time, UtcOffset};

use crate::client::TmdbClient;
use crate::error::TmdbError;
use crate::model::{Episode, EpisodeGroup, EpisodeGroupKind};

/// What `/settings` renders for TMDB.
///
/// One field, and the label names *which* credential: the v4 read access
/// token and the v3 API key are different strings, and passing the v3 key
/// as a bearer yields a 401 that looks exactly like a wrong token.
const CREDENTIALS: &[CredentialField] = &[CredentialField {
    key: "tmdb_token",
    label: "Token de leitura v4 do TMDB",
    secret: true,
    required: true,
}];

impl MetadataProvider for TmdbClient {
    fn source(&self) -> MetadataSource {
        MetadataSource::Tmdb
    }

    /// Identity for both kinds; structure for series only, because a
    /// film has no seasons.
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            identity: MediaSupport::Both,
            structure: MediaSupport::Series,
            descriptive: MediaSupport::Both,
        }
    }

    fn credentials(&self) -> &'static [CredentialField] {
        CREDENTIALS
    }

    fn verify(&self) -> MetaFuture<'_, Result<(), MetadataError>> {
        Box::pin(async move { self.verify_token().await.map_err(translate) })
    }

    fn find(
        &self,
        known: &ExternalId,
        media: MediaType,
    ) -> MetaFuture<'_, Result<Option<ExternalId>, MetadataError>> {
        let known = known.clone();
        Box::pin(async move {
            // Asked with its own id, the honest answer is that id. Saying
            // so here means a caller resolving a set of ids does not have
            // to special-case the provider it already has one for.
            if known.source() == MetadataSource::Tmdb {
                return Ok(Some(known));
            }
            let found = match known.source() {
                MetadataSource::Imdb => self.find_by_imdb(known.value()).await,
                MetadataSource::Tvdb => {
                    let numeric = i64::from(known.as_u32().map_err(MetadataError::BadId)?);
                    self.find_by_tvdb(numeric).await
                }
                // `Tmdb` returned above. Spelled out rather than
                // wildcarded so a new source has to be placed here
                // deliberately instead of silently finding nothing.
                MetadataSource::Tmdb => {
                    return Err(MetadataError::Unsupported {
                        origin: MetadataSource::Tmdb,
                        capability: "find",
                        media,
                    });
                }
            }
            .map_err(translate)?;

            let matched = match media {
                MediaType::Movie => found.movies.first().map(|m| m.tmdb_id),
                MediaType::Tv => found.series.first().map(|s| s.tmdb_id),
            };
            matched
                .map(|id| ExternalId::new(MetadataSource::Tmdb, &id.to_string()))
                .transpose()
                .map_err(MetadataError::BadId)
        })
    }

    /// The episode groups that renumber, and only those.
    ///
    /// TMDB's `type` has seven values and three of them — original air
    /// date, TV, and anything else canonical — describe the ordering the
    /// tree already has. Offering those would offer what is already
    /// there, so [`EpisodeGroupKind::is_alternate_ordering`] filters
    /// them out. On this operator's catalogue the common answer is an
    /// empty list.
    fn variants(
        &self,
        series: &ExternalId,
    ) -> MetaFuture<'_, Result<Vec<StructureVariant>, MetadataError>> {
        let series = series.clone();
        Box::pin(async move {
            let numeric = i64::from(series.as_u32().map_err(MetadataError::BadId)?);
            let groups = self.episode_groups(numeric).await.map_err(translate)?;
            Ok(groups
                .into_iter()
                .filter(|g| g.kind.is_alternate_ordering())
                .map(|g| StructureVariant {
                    family: family_of(g.kind),
                    handle: g.id,
                    name: g
                        .name
                        .filter(|n| !n.trim().is_empty())
                        .unwrap_or_else(|| "ordenação sem nome".to_owned()),
                    // TMDB reports how many episodes a group covers, and
                    // it is routinely fewer than the series has — a story
                    // arc group can cover 48 of 59. The screen has to be
                    // able to say so before the operator picks it.
                    coverage: u32::try_from(g.episode_count)
                        .ok()
                        .map(|covered| (covered, covered)),
                })
                .collect())
        })
    }

    /// TMDB describes both kinds, and its client already walks the
    /// pt-BR → pt-PT → en-US fallback in `model.rs` — TMDB has no
    /// automatic language fallback, so `language=pt-BR` returns an
    /// *empty* overview rather than an English one.
    fn describe(
        &self,
        id: &ExternalId,
        media: MediaType,
    ) -> MetaFuture<'_, Result<Description, MetadataError>> {
        let id = id.clone();
        Box::pin(async move {
            let numeric = i64::from(id.as_u32().map_err(MetadataError::BadId)?);
            match media {
                MediaType::Movie => {
                    let d = self.movie(numeric).await.map_err(translate)?;
                    Ok(Description {
                        original_title: d.original_title.clone(),
                        overview: d.overview.clone(),
                        year: d.release_date.map(Date::year),
                        status: status_of(d.status.as_deref()),
                        runtime_minutes: d.runtime_minutes,
                        poster: art(d.poster_path.clone()),
                        backdrop: art(d.backdrop_path.clone()),
                        digital_release_at: d.digital_release.map(at_midnight),
                        physical_release_at: d.physical_release.map(at_midnight),
                        ..Description::new(MetadataSource::Tmdb, d.title)
                    })
                }
                MediaType::Tv => {
                    let d = self.tv(numeric).await.map_err(translate)?;
                    Ok(Description {
                        original_title: d.original_name.clone(),
                        overview: d.overview.clone(),
                        year: d.first_air_date.map(Date::year),
                        status: status_of(d.status.as_deref()),
                        runtime_minutes: d.episode_runtime,
                        poster: art(d.poster_path.clone()),
                        backdrop: art(d.backdrop_path.clone()),
                        next_air_date: d.next_air_date.map(at_midnight),
                        ..Description::new(MetadataSource::Tmdb, d.name)
                    })
                }
            }
        })
    }

    fn tree(
        &self,
        series: &ExternalId,
        ordering: &Ordering,
    ) -> MetaFuture<'_, Result<SeriesTree, MetadataError>> {
        let series = series.clone();
        let ordering = ordering.clone();
        Box::pin(async move {
            let numeric = i64::from(series.as_u32().map_err(MetadataError::BadId)?);
            let canonical = self.canonical_tree(numeric, &series).await?;

            match &ordering {
                Ordering::Default => Ok(SeriesTree {
                    ordering,
                    ..canonical
                }),
                Ordering::Manual { blocks } => {
                    canonical
                        .recut(blocks)
                        .map_err(|e| MetadataError::Malformed {
                            origin: MetadataSource::Tmdb,
                            detail: e.to_string(),
                        })
                }
                Ordering::Named { family, handle } => {
                    let group = self.episode_group(handle).await.map_err(|e| match e {
                        TmdbError::NotFound(_) => MetadataError::UnknownOrdering {
                            origin: MetadataSource::Tmdb,
                            id: series.value().to_owned(),
                            handle: handle.to_string(),
                        },
                        other => translate(other),
                    })?;
                    let tree = tree_from_group(&group, &canonical, *family);
                    if tree.seasons.iter().all(|s| s.episodes.is_empty()) {
                        return Err(MetadataError::Empty {
                            origin: MetadataSource::Tmdb,
                            id: series.value().to_owned(),
                        });
                    }
                    Ok(tree)
                }
            }
        })
    }
}

impl TmdbClient {
    /// The series as `/tv/{id}` plus one call per season numbers it.
    async fn canonical_tree(
        &self,
        numeric: i64,
        series: &ExternalId,
    ) -> Result<SeriesTree, MetadataError> {
        let details = self.tv(numeric).await.map_err(translate)?;
        let mut seasons = Vec::with_capacity(details.seasons.len());
        for summary in &details.seasons {
            let full = self
                .season(numeric, summary.season_number)
                .await
                .map_err(translate)?;
            seasons.push(TreeSeason {
                number: full.season_number,
                air_date: full.air_date.or(summary.air_date).map(midnight_utc),
                episodes: full.episodes.iter().map(episode_of).collect(),
            });
        }
        let tree = SeriesTree {
            source: MetadataSource::Tmdb,
            ordering: Ordering::Default,
            seasons,
        };
        if tree.episode_count() == 0 {
            return Err(MetadataError::Empty {
                origin: MetadataSource::Tmdb,
                id: series.value().to_owned(),
            });
        }
        Ok(tree)
    }
}

/// Build the tree an episode group describes.
///
/// **Blocks are 1-based and episodes are 0-based within their block** —
/// verified against a live capture of Jujutsu Kaisen's group, whose
/// buckets run 1/2/3 with 24/23/12 episodes and whose `order` restarts at
/// 0 inside every bucket. Reading `order` as global would put every
/// episode after the first bucket on the wrong season.
///
/// A group carries no air date, so each episode's is joined from the
/// canonical tree by TMDB's own episode id — the identity that is stable
/// across orderings. Without it the air date, which is the one value that
/// belongs to the episode rather than to a numbering scheme, would be
/// missing exactly where it is most needed.
///
/// Episodes the group does not list keep no place here: a group covering
/// 48 of 59 describes 48, and inventing the other 11 would put them under
/// numbers no release uses.
fn tree_from_group(
    group: &EpisodeGroup,
    canonical: &SeriesTree,
    family: OrderingFamily,
) -> SeriesTree {
    let dated: HashMap<&str, &TreeEpisode> = canonical
        .seasons
        .iter()
        .flat_map(|s| s.episodes.iter())
        .map(|e| (e.external_id.as_str(), e))
        .collect();

    let seasons = group
        .groups
        .iter()
        .enumerate()
        .map(|(index, part)| {
            let number = if part.order > 0 {
                part.order
            } else {
                i32::try_from(index).unwrap_or(0) + 1
            };
            let episodes: Vec<TreeEpisode> = part
                .episodes
                .iter()
                .enumerate()
                .map(|(position, episode)| {
                    let within = if episode.order >= 0 {
                        episode.order + 1
                    } else {
                        i32::try_from(position).unwrap_or(0) + 1
                    };
                    let id = episode.id.to_string();
                    let known = dated.get(id.as_str());
                    TreeEpisode {
                        number: within,
                        title: episode
                            .title
                            .clone()
                            .or_else(|| known.and_then(|e| e.title.clone())),
                        air_date: known.and_then(|e| e.air_date),
                        absolute_number: None,
                        external_id: id,
                    }
                })
                .collect();
            TreeSeason {
                number,
                air_date: episodes.iter().filter_map(|e| e.air_date).min(),
                episodes,
            }
        })
        .collect();

    SeriesTree {
        source: MetadataSource::Tmdb,
        ordering: Ordering::Named {
            family,
            handle: group.id.clone().into_boxed_str(),
        },
        seasons,
    }
}

/// brarr's word for one of TMDB's group types.
///
/// `Other` catches the kinds this workspace has no word for, which is
/// what keeps the accompanying CHECK from needing a migration every time
/// TMDB adds one.
fn family_of(kind: EpisodeGroupKind) -> OrderingFamily {
    match kind {
        EpisodeGroupKind::Absolute => OrderingFamily::Absolute,
        EpisodeGroupKind::Dvd => OrderingFamily::Dvd,
        EpisodeGroupKind::StoryArc => OrderingFamily::Alternate,
        EpisodeGroupKind::Production => OrderingFamily::Production,
        EpisodeGroupKind::OriginalAirDate | EpisodeGroupKind::Tv => OrderingFamily::Aired,
        EpisodeGroupKind::Digital | EpisodeGroupKind::Other(_) => OrderingFamily::Other,
    }
}

fn episode_of(episode: &Episode) -> TreeEpisode {
    TreeEpisode {
        external_id: episode.id.to_string(),
        number: episode.episode_number,
        title: episode.title.clone(),
        air_date: episode.air_date.map(midnight_utc),
        // TMDB has no absolute axis on the season endpoint. `None` says
        // so; a computed running total would look like data.
        absolute_number: None,
    }
}

/// TMDB dates are calendar days with no time. Midnight UTC is the same
/// convention `brarr-tvdb` already uses, so the two are comparable.
fn midnight_utc(date: Date) -> OffsetDateTime {
    OffsetDateTime::new_in_offset(date, Time::MIDNIGHT, UtcOffset::UTC)
}

/// Peel this crate's error into the neutral one.
/// A CDN-relative path, tagged so the caller knows not to treat it as a
/// URL. `TheTVDB` returns absolute ones; prefixing the TMDB CDN onto those
/// 404s in silence.
fn art(path: Option<String>) -> Option<Artwork> {
    path.filter(|p| !p.trim().is_empty()).map(|value| Artwork {
        source: MetadataSource::Tmdb,
        value,
    })
}

/// TMDB's status words, in brarr's vocabulary.
///
/// **One provider's dialect, mapped at that provider's boundary.**
/// `TheTVDB` says `Continuing` for what TMDB calls `Returning Series`, and
/// a field that accepts both is a field whose comparisons depend on who
/// wrote it. An unrecognised word reads as unknown rather than being
/// carried through raw.
fn status_of(raw: Option<&str>) -> Option<ProductionStatus> {
    match raw?.trim() {
        "Returning Series" => Some(ProductionStatus::Returning),
        "Ended" => Some(ProductionStatus::Ended),
        "Canceled" | "Cancelled" => Some(ProductionStatus::Cancelled),
        "In Production" | "Post Production" | "Planned" => Some(ProductionStatus::InProduction),
        "Released" => Some(ProductionStatus::Released),
        "Rumored" => Some(ProductionStatus::Announced),
        _ => None,
    }
}

/// A date at midnight UTC — the convention `brarr-tmdb`, `brarr-tvdb`
/// and the \*arr bridge share, so the three produce comparable values.
fn at_midnight(date: Date) -> OffsetDateTime {
    OffsetDateTime::new_in_offset(date, Time::MIDNIGHT, UtcOffset::UTC)
}

fn translate(e: TmdbError) -> MetadataError {
    let origin = MetadataSource::Tmdb;
    match e {
        // The mix-up worth naming: the v3 API key is a different string
        // from the v4 read access token, and sending it as a bearer is a
        // 401 that looks like a revoked credential.
        TmdbError::Unauthorized | TmdbError::InvalidToken => MetadataError::Unauthorized { origin },
        TmdbError::NotFound(what) => MetadataError::NotFound {
            origin,
            media: MediaType::Tv,
            id: what,
        },
        TmdbError::RateLimited | TmdbError::Http(_) => MetadataError::Unavailable {
            origin,
            detail: e.to_string(),
        },
        TmdbError::BadJson(_) | TmdbError::BadUrl(_) | TmdbError::ClientBuild(_) => {
            MetadataError::Malformed {
                origin,
                detail: e.to_string(),
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;
    use crate::model::{EpisodeGroupPart, GroupEpisode};
    use time::macros::date;

    fn canonical(episodes: &[(i64, i32, i32)]) -> SeriesTree {
        let mut seasons: Vec<TreeSeason> = Vec::new();
        for (id, season, number) in episodes {
            let episode = TreeEpisode {
                external_id: id.to_string(),
                number: *number,
                title: Some(format!("E{number}")),
                air_date: Some(midnight_utc(date!(2020 - 10 - 03))),
                absolute_number: None,
            };
            if let Some(existing) = seasons.iter_mut().find(|s| s.number == *season) {
                existing.episodes.push(episode);
            } else {
                seasons.push(TreeSeason {
                    number: *season,
                    air_date: None,
                    episodes: vec![episode],
                });
            }
        }
        SeriesTree {
            source: MetadataSource::Tmdb,
            ordering: Ordering::Default,
            seasons,
        }
    }

    /// **Jujutsu Kaisen's real shape.** TMDB flattens it to one season of
    /// 59 and its `季` group cuts it 24/23/12, which is what every
    /// release is named after. Buckets are 1-based, positions inside a
    /// bucket are 0-based, and reading the second as global would put
    /// every episode after the first bucket on the wrong season.
    #[test]
    fn a_group_becomes_the_tree_releases_are_named_after() {
        let flat: Vec<(i64, i32, i32)> = (1..=59).map(|n| (i64::from(n), 1, n)).collect();
        let canonical = canonical(&flat);

        let mut groups = Vec::new();
        let mut next = 1_i64;
        for (order, size) in [(1, 24), (2, 23), (3, 12)] {
            groups.push(EpisodeGroupPart {
                name: Some(format!("Parte {order}")),
                order,
                episodes: (0..size)
                    .map(|position| {
                        let id = next;
                        next += 1;
                        GroupEpisode {
                            id,
                            season_number: 1,
                            episode_number: i32::try_from(id).unwrap_or(0),
                            order: position,
                            title: None,
                        }
                    })
                    .collect(),
            });
        }
        let group = EpisodeGroup {
            id: "6961c83d72e76980b8bd3780".to_owned(),
            name: Some("季".to_owned()),
            kind: EpisodeGroupKind::StoryArc,
            groups,
        };

        let tree = tree_from_group(&group, &canonical, OrderingFamily::Alternate);
        assert_eq!(tree.shape(), vec![24, 23, 12]);
        assert_eq!(tree.episode_count(), 59);

        // The coordinate releases use, produced directly: S02E23.
        let second = tree.seasons.iter().find(|s| s.number == 2).unwrap();
        assert_eq!(second.episodes[22].number, 23);
        // …and it is still canonical episode 47, because the identity
        // did not move.
        assert_eq!(second.episodes[22].external_id, "47");
        // The air date is joined from the canonical tree, since a group
        // carries none — and it is the arbiter when two providers
        // disagree, so losing it would matter most exactly here.
        assert!(second.episodes[22].air_date.is_some());
        assert_eq!(
            tree.ordering.handle(),
            Some("6961c83d72e76980b8bd3780"),
            "the handle has to survive, or a stored ordering stops resolving"
        );
    }

    /// A group covering part of a series describes that part. Filling in
    /// the rest would put episodes under numbers no release uses.
    #[test]
    fn a_partial_group_covers_what_it_lists() {
        let canonical = canonical(&[(1, 1, 1), (2, 1, 2), (3, 1, 3)]);
        let group = EpisodeGroup {
            id: "g".to_owned(),
            name: None,
            kind: EpisodeGroupKind::StoryArc,
            groups: vec![EpisodeGroupPart {
                name: None,
                order: 1,
                episodes: vec![GroupEpisode {
                    id: 1,
                    season_number: 1,
                    episode_number: 1,
                    order: 0,
                    title: None,
                }],
            }],
        };
        let tree = tree_from_group(&group, &canonical, OrderingFamily::Alternate);
        assert_eq!(tree.episode_count(), 1);
    }

    /// A bucket with no usable order falls back to its position rather
    /// than collapsing every bucket onto season 0.
    #[test]
    fn a_bucket_without_an_order_takes_its_position() {
        let canonical = canonical(&[(1, 1, 1), (2, 1, 2)]);
        let group = EpisodeGroup {
            id: "g".to_owned(),
            name: None,
            kind: EpisodeGroupKind::Absolute,
            groups: vec![
                EpisodeGroupPart {
                    name: None,
                    order: 0,
                    episodes: vec![GroupEpisode {
                        id: 1,
                        season_number: 1,
                        episode_number: 1,
                        order: 0,
                        title: None,
                    }],
                },
                EpisodeGroupPart {
                    name: None,
                    order: 0,
                    episodes: vec![GroupEpisode {
                        id: 2,
                        season_number: 1,
                        episode_number: 2,
                        order: 0,
                        title: None,
                    }],
                },
            ],
        };
        let tree = tree_from_group(&group, &canonical, OrderingFamily::Absolute);
        assert_eq!(
            tree.seasons.iter().map(|s| s.number).collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    /// Every group kind maps to a word, and the canonical ones map to a
    /// family that is not offered as a choice.
    #[test]
    fn every_group_kind_has_a_family() {
        assert_eq!(
            family_of(EpisodeGroupKind::Absolute),
            OrderingFamily::Absolute
        );
        assert_eq!(family_of(EpisodeGroupKind::Dvd), OrderingFamily::Dvd);
        assert_eq!(
            family_of(EpisodeGroupKind::StoryArc),
            OrderingFamily::Alternate
        );
        assert!(!family_of(EpisodeGroupKind::OriginalAirDate).renumbers());
        assert!(!family_of(EpisodeGroupKind::Tv).renumbers());
        assert!(family_of(EpisodeGroupKind::Absolute).renumbers());
    }

    #[test]
    fn capabilities_are_coherent() {
        let client = TmdbClient::new("token").unwrap();
        let caps = MetadataProvider::capabilities(&client);
        assert!(caps.is_coherent());
        assert!(caps.identity.covers(MediaType::Movie));
        assert!(caps.identity.covers(MediaType::Tv));
        assert!(
            !caps.structure.covers(MediaType::Movie),
            "a film has no tree"
        );
    }

    /// The credential mix-up this crate exists to prevent has to survive
    /// the translation: a v3 key sent as a bearer is a 401, and the
    /// operator has to be told which string is wrong.
    #[test]
    fn a_refused_credential_keeps_its_identity() {
        assert!(matches!(
            translate(TmdbError::Unauthorized),
            MetadataError::Unauthorized { .. }
        ));
        assert!(matches!(
            translate(TmdbError::InvalidToken),
            MetadataError::Unauthorized { .. }
        ));
        assert!(!translate(TmdbError::Unauthorized).is_transient());
        assert!(translate(TmdbError::RateLimited).is_transient());
    }

    /// Dates from the two providers have to be comparable, or the air
    /// date cannot arbitrate between them.
    #[test]
    fn a_tmdb_date_becomes_midnight_utc() {
        let at = midnight_utc(date!(2015 - 10 - 18));
        assert_eq!(at.offset(), UtcOffset::UTC);
        assert_eq!(at.time(), Time::MIDNIGHT);
    }
}
