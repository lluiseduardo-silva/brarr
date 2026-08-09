//! Public types returned by the client, plus the language and
//! release-date resolution that turns TMDB's shape into brarr's.
//!
//! Two upstream behaviours drive most of this module:
//!
//! - **There is no automatic language fallback.** Asking for
//!   `language=pt-BR` returns the localised strings when a translation
//!   exists and an *empty* `overview` when it does not (the title falls
//!   back to the original). So the client also asks for
//!   `append_to_response=translations` and walks pt-BR → pt-PT → en-US
//!   in code.
//! - **Release dates are per country and per type.** The digital date —
//!   the one that decides when searching is no longer wasted effort —
//!   lives at `release_dates.results[country].release_dates[type == 4]`.

use time::Date;

use crate::dto;

/// TMDB release-date types worth naming. The full list also covers
/// premiere (1), limited theatrical (2) and TV (6), which brarr ignores.
const TYPE_THEATRICAL: i64 = 3;
const TYPE_DIGITAL: i64 = 4;
const TYPE_PHYSICAL: i64 = 5;

/// Language preference walked when a field comes back empty. Ordered.
const LANGUAGE_FALLBACK: [(&str, &str); 3] = [("pt", "BR"), ("pt", "PT"), ("en", "US")];

/// A movie as it appears in search results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MovieSummary {
    /// TMDB id.
    pub tmdb_id: i64,
    /// Localised title.
    pub title: String,
    /// Original-language title.
    pub original_title: Option<String>,
    /// Synopsis; `None` when no translation exists in the fallback chain.
    pub overview: Option<String>,
    /// Poster path relative to the image CDN.
    pub poster_path: Option<String>,
    /// Backdrop path.
    pub backdrop_path: Option<String>,
    /// Theatrical release date as TMDB reports it on the summary.
    pub release_date: Option<Date>,
}

impl MovieSummary {
    /// Release year, for the "Duna: Parte Dois (2024)" form.
    #[must_use]
    pub fn year(&self) -> Option<i32> {
        self.release_date.map(time::Date::year)
    }
}

/// A series as it appears in search results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TvSummary {
    /// TMDB id.
    pub tmdb_id: i64,
    /// Localised name.
    pub name: String,
    /// Original-language name.
    pub original_name: Option<String>,
    /// Synopsis.
    pub overview: Option<String>,
    /// Poster path.
    pub poster_path: Option<String>,
    /// Backdrop path.
    pub backdrop_path: Option<String>,
    /// First air date.
    pub first_air_date: Option<Date>,
}

impl TvSummary {
    /// First-air year.
    #[must_use]
    pub fn year(&self) -> Option<i32> {
        self.first_air_date.map(time::Date::year)
    }
}

/// Full movie record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MovieDetails {
    /// TMDB id.
    pub tmdb_id: i64,
    /// Canonical `ttNNNNNNN`, prefix included.
    pub imdb_id: Option<String>,
    /// Localised title, after the fallback walk.
    pub title: String,
    /// Original-language title.
    pub original_title: Option<String>,
    /// Synopsis, after the fallback walk.
    pub overview: Option<String>,
    /// Poster path.
    pub poster_path: Option<String>,
    /// Backdrop path.
    pub backdrop_path: Option<String>,
    /// Theatrical release date.
    pub release_date: Option<Date>,
    /// Digital release (type 4) for the preferred country. Until this
    /// date passes, searching usually only finds cams.
    pub digital_release: Option<Date>,
    /// Physical release (type 5).
    pub physical_release: Option<Date>,
    /// Runtime in minutes.
    pub runtime_minutes: Option<i32>,
    /// TMDB status string (`Released`, `Post Production`, …).
    pub status: Option<String>,
}

/// One entry of a series' season list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeasonSummary {
    /// Season number; `0` is TMDB's specials season.
    pub season_number: i32,
    /// Episodes TMDB reports for the season.
    pub episode_count: i32,
    /// Season air date.
    pub air_date: Option<Date>,
}

/// Full series record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TvDetails {
    /// TMDB id.
    pub tmdb_id: i64,
    /// Canonical `ttNNNNNNN`.
    pub imdb_id: Option<String>,
    /// TVDB id — series only; TMDB never exposes one for movies.
    pub tvdb_id: Option<i64>,
    /// Localised name.
    pub name: String,
    /// Original-language name.
    pub original_name: Option<String>,
    /// Synopsis.
    pub overview: Option<String>,
    /// Poster path.
    pub poster_path: Option<String>,
    /// Backdrop path.
    pub backdrop_path: Option<String>,
    /// First air date.
    pub first_air_date: Option<Date>,
    /// TMDB status (`Returning Series`, `Ended`, `Canceled`, …).
    pub status: Option<String>,
    /// Whether TMDB still considers it in production.
    pub in_production: bool,
    /// Air date of the next unaired episode.
    pub next_air_date: Option<Date>,
    /// Typical episode runtime.
    pub episode_runtime: Option<i32>,
    /// Season list, ascending.
    pub seasons: Vec<SeasonSummary>,
}

/// One episode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Episode {
    /// TMDB's own episode id — stable across orderings, and the only
    /// identity that is.  when the payload omitted it.
    pub id: i64,
    /// Season it belongs to.
    pub season_number: i32,
    /// Number within the season.
    pub episode_number: i32,
    /// Episode title.
    pub title: Option<String>,
    /// Air date.
    pub air_date: Option<Date>,
}

/// One season with its episodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeasonDetails {
    /// Season number.
    pub season_number: i32,
    /// Season air date.
    pub air_date: Option<Date>,
    /// Episodes, in the order TMDB returned them.
    pub episodes: Vec<Episode>,
}

/// What kind of ordering an episode group expresses.
///
/// TMDB's `type` is an integer with seven documented values. Two matter
/// to brarr: [`Self::Absolute`], which is how an anime release is
/// numbered (`Yu-Gi-Oh! - 224`, no `SxxEyy` anywhere), and
/// [`Self::StoryArc`], which is how a long-running series is sometimes
/// published. The rest are recorded rather than dropped so the screen
/// can say what it found instead of hiding it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpisodeGroupKind {
    /// 1 — original air date.
    OriginalAirDate,
    /// 2 — absolute numbering, one run from episode 1.
    Absolute,
    /// 3 — DVD ordering.
    Dvd,
    /// 4 — digital ordering.
    Digital,
    /// 5 — story arc.
    StoryArc,
    /// 6 — production order.
    Production,
    /// 7 — TV ordering.
    Tv,
    /// A value added upstream after this was written. Kept rather than
    /// collapsed into a default, so an unknown ordering is visible as
    /// unknown.
    Other(i64),
}

impl EpisodeGroupKind {
    /// Map TMDB's integer.
    #[must_use]
    pub fn from_code(code: i64) -> Self {
        match code {
            1 => Self::OriginalAirDate,
            2 => Self::Absolute,
            3 => Self::Dvd,
            4 => Self::Digital,
            5 => Self::StoryArc,
            6 => Self::Production,
            7 => Self::Tv,
            other => Self::Other(other),
        }
    }

    /// Whether this ordering renumbers episodes away from the canonical
    /// `(season, episode)` — i.e. whether it is one brarr could search
    /// under. Original air date and TV order are the canonical shape.
    #[must_use]
    pub fn is_alternate_ordering(self) -> bool {
        matches!(
            self,
            Self::Absolute | Self::StoryArc | Self::Dvd | Self::Digital | Self::Production
        )
    }
}

/// One ordering TMDB knows for a series, as the list endpoint gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpisodeGroupSummary {
    /// TMDB's id for the group — a hex string, not an integer.
    pub id: String,
    /// Name the contributor gave it.
    pub name: Option<String>,
    /// Free-text description, often empty.
    pub description: Option<String>,
    /// What kind of ordering it is.
    pub kind: EpisodeGroupKind,
    /// How many buckets it has.
    pub group_count: i32,
    /// How many episodes it covers.
    pub episode_count: i32,
}

/// One ordering with its contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpisodeGroup {
    /// TMDB's id for the group.
    pub id: String,
    /// Name the contributor gave it.
    pub name: Option<String>,
    /// What kind of ordering it is.
    pub kind: EpisodeGroupKind,
    /// Its buckets, in the order TMDB returned them.
    pub groups: Vec<EpisodeGroupPart>,
}

/// One bucket inside an ordering.
///
/// **Not a TMDB season.** It has no season id, which is why an alternate
/// ordering cannot simply be keyed onto `library_seasons`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpisodeGroupPart {
    /// Bucket name — an arc title, or "Season 1" in DVD order.
    pub name: Option<String>,
    /// Position of the bucket within the group.
    pub order: i32,
    /// Episodes in it.
    pub episodes: Vec<GroupEpisode>,
}

/// An episode as it appears inside a group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupEpisode {
    /// **TMDB's own episode id.** The identity that is stable across
    /// orderings — and the one `EpisodeDto` discards on the season
    /// endpoint, which is why re-numbering a series is currently a
    /// rebuild rather than a relabel.
    pub id: i64,
    /// Canonical season, as `/tv/{id}/season/{n}` numbers it.
    pub season_number: i32,
    /// Canonical number within that season.
    pub episode_number: i32,
    /// Position within this group — the alternate numbering.
    pub order: i32,
    /// Episode title.
    pub title: Option<String>,
}

/// What `/find` matched for an external id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FindResults {
    /// Movies matching the external id.
    pub movies: Vec<MovieSummary>,
    /// Series matching the external id.
    pub series: Vec<TvSummary>,
}

impl EpisodeGroupSummary {
    pub(crate) fn from_dto(dto: dto::EpisodeGroupSummaryDto) -> Self {
        Self {
            id: dto.id,
            name: dto.name,
            description: dto.description,
            kind: EpisodeGroupKind::from_code(dto.kind),
            group_count: i32::try_from(dto.group_count).unwrap_or(0),
            episode_count: i32::try_from(dto.episode_count).unwrap_or(0),
        }
    }
}

impl EpisodeGroup {
    pub(crate) fn from_dto(dto: dto::EpisodeGroupDto) -> Self {
        Self {
            id: dto.id,
            name: dto.name,
            kind: EpisodeGroupKind::from_code(dto.kind),
            groups: dto
                .groups
                .into_iter()
                .map(|g| EpisodeGroupPart {
                    name: g.name,
                    order: i32::try_from(g.order).unwrap_or(0),
                    episodes: g
                        .episodes
                        .into_iter()
                        .map(|e| GroupEpisode {
                            id: e.id,
                            season_number: i32::try_from(e.season_number).unwrap_or(0),
                            episode_number: i32::try_from(e.episode_number).unwrap_or(0),
                            order: i32::try_from(e.order).unwrap_or(0),
                            title: e.name,
                        })
                        .collect(),
                })
                .collect(),
        }
    }
}

/// Pick the best translation for a field, walking pt-BR → pt-PT → en-US.
///
/// `pick` selects which of `title` / `name` / `overview` to read, since
/// TMDB names the movie field `title` and the series field `name` inside
/// the very same translation payload.
fn from_translations<F>(translations: &dto::TranslationsDto, pick: F) -> Option<String>
where
    F: Fn(&dto::TranslationDataDto) -> Option<&String>,
{
    for (lang, country) in LANGUAGE_FALLBACK {
        let hit = translations.translations.iter().find(|t| {
            t.iso_639_1.eq_ignore_ascii_case(lang) && t.iso_3166_1.eq_ignore_ascii_case(country)
        });
        if let Some(found) = hit
            && let Some(value) = pick(&found.data)
        {
            return Some(value.clone());
        }
    }
    None
}

/// Extract a release date of `kind` for the preferred country, falling
/// back to US and then giving up.
///
/// Deliberately **no** "any country" fallback. The live data makes the
/// reason concrete: The Matrix reports no digital date for BR or US, but
/// does for AE (2016-01-07). A date from an unrelated market says nothing
/// about when a release becomes findable here, and this value gates
/// whether searching is worth attempting at all — a confident wrong
/// answer is worse than `None`.
fn release_date_of(
    dates: &dto::ReleaseDatesDto,
    preferred_country: &str,
    kind: i64,
) -> Option<Date> {
    let find_in = |country: &str| -> Option<Date> {
        dates
            .results
            .iter()
            .find(|c| c.iso_3166_1.eq_ignore_ascii_case(country))
            .and_then(|c| {
                c.release_dates
                    .iter()
                    .find(|e| e.kind == kind)
                    .and_then(|e| e.release_date)
            })
    };
    find_in(preferred_country).or_else(|| find_in("US"))
}

impl MovieSummary {
    pub(crate) fn from_dto(dto: dto::MovieSummaryDto) -> Self {
        Self {
            tmdb_id: dto.id,
            title: dto.title,
            original_title: dto.original_title,
            overview: dto.overview,
            poster_path: dto.poster_path,
            backdrop_path: dto.backdrop_path,
            release_date: dto.release_date,
        }
    }
}

impl TvSummary {
    pub(crate) fn from_dto(dto: dto::TvSummaryDto) -> Self {
        Self {
            tmdb_id: dto.id,
            name: dto.name,
            original_name: dto.original_name,
            overview: dto.overview,
            poster_path: dto.poster_path,
            backdrop_path: dto.backdrop_path,
            first_air_date: dto.first_air_date,
        }
    }
}

impl MovieDetails {
    pub(crate) fn from_dto(dto: dto::MovieDetailsDto, preferred_country: &str) -> Self {
        // The top-level `imdb_id` and the appended `external_ids.imdb_id`
        // carry the same value; prefer whichever is present.
        let imdb_id = dto.imdb_id.or(dto.external_ids.imdb_id);
        let overview = dto
            .overview
            .or_else(|| from_translations(&dto.translations, |d| d.overview.as_ref()));
        let title = if dto.title.trim().is_empty() {
            from_translations(&dto.translations, |d| d.title.as_ref())
                .or_else(|| dto.original_title.clone())
                .unwrap_or_default()
        } else {
            dto.title
        };
        Self {
            tmdb_id: dto.id,
            imdb_id,
            title,
            original_title: dto.original_title,
            overview,
            poster_path: dto.poster_path,
            backdrop_path: dto.backdrop_path,
            release_date: dto.release_date.or_else(|| {
                release_date_of(&dto.release_dates, preferred_country, TYPE_THEATRICAL)
            }),
            digital_release: release_date_of(&dto.release_dates, preferred_country, TYPE_DIGITAL),
            physical_release: release_date_of(&dto.release_dates, preferred_country, TYPE_PHYSICAL),
            runtime_minutes: dto.runtime.and_then(|r| i32::try_from(r).ok()),
            status: dto.status,
        }
    }
}

impl TvDetails {
    pub(crate) fn from_dto(dto: dto::TvDetailsDto) -> Self {
        let overview = dto
            .overview
            .or_else(|| from_translations(&dto.translations, |d| d.overview.as_ref()));
        let name = if dto.name.trim().is_empty() {
            from_translations(&dto.translations, |d| d.name.as_ref())
                .or_else(|| dto.original_name.clone())
                .unwrap_or_default()
        } else {
            dto.name
        };
        let mut seasons: Vec<SeasonSummary> = dto
            .seasons
            .into_iter()
            .map(|s| SeasonSummary {
                season_number: i32::try_from(s.season_number).unwrap_or(0),
                episode_count: i32::try_from(s.episode_count).unwrap_or(0),
                air_date: s.air_date,
            })
            .collect();
        seasons.sort_by_key(|s| s.season_number);
        Self {
            tmdb_id: dto.id,
            imdb_id: dto.external_ids.imdb_id,
            tvdb_id: dto.external_ids.tvdb_id,
            name,
            original_name: dto.original_name,
            overview,
            poster_path: dto.poster_path,
            backdrop_path: dto.backdrop_path,
            first_air_date: dto.first_air_date,
            status: dto.status,
            in_production: dto.in_production,
            next_air_date: dto.next_episode_to_air.and_then(|n| n.air_date),
            episode_runtime: dto
                .episode_run_time
                .first()
                .and_then(|r| i32::try_from(*r).ok()),
            seasons,
        }
    }
}

impl SeasonDetails {
    pub(crate) fn from_dto(dto: dto::SeasonDetailsDto) -> Self {
        Self {
            season_number: i32::try_from(dto.season_number).unwrap_or(0),
            air_date: dto.air_date,
            episodes: dto
                .episodes
                .into_iter()
                .map(|e| Episode {
                    id: e.id,
                    season_number: i32::try_from(e.season_number).unwrap_or(0),
                    episode_number: i32::try_from(e.episode_number).unwrap_or(0),
                    title: e.name,
                    air_date: e.air_date,
                })
                .collect(),
        }
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
    use time::macros::date;

    fn translations(entries: &[(&str, &str, Option<&str>, Option<&str>)]) -> dto::TranslationsDto {
        dto::TranslationsDto {
            translations: entries
                .iter()
                .map(|(lang, country, title, overview)| dto::TranslationDto {
                    iso_639_1: (*lang).to_owned(),
                    iso_3166_1: (*country).to_owned(),
                    data: dto::TranslationDataDto {
                        title: title.map(str::to_owned),
                        name: title.map(str::to_owned),
                        overview: overview.map(str::to_owned),
                    },
                })
                .collect(),
        }
    }

    #[test]
    fn pt_br_wins_when_present() {
        let t = translations(&[
            ("en", "US", Some("The Matrix"), Some("A hacker learns…")),
            ("pt", "BR", Some("Matrix"), Some("Um hacker descobre…")),
            ("pt", "PT", Some("Matrix"), Some("Um pirata informático…")),
        ]);
        assert_eq!(
            from_translations(&t, |d| d.overview.as_ref()).as_deref(),
            Some("Um hacker descobre…")
        );
    }

    #[test]
    fn falls_back_to_pt_pt_then_en_us() {
        let only_pt_pt = translations(&[
            ("en", "US", None, Some("english")),
            ("pt", "PT", None, Some("português europeu")),
        ]);
        assert_eq!(
            from_translations(&only_pt_pt, |d| d.overview.as_ref()).as_deref(),
            Some("português europeu")
        );

        let only_en = translations(&[("en", "US", None, Some("english"))]);
        assert_eq!(
            from_translations(&only_en, |d| d.overview.as_ref()).as_deref(),
            Some("english")
        );
    }

    #[test]
    fn no_usable_translation_yields_none() {
        let t = translations(&[("de", "DE", None, Some("deutsch"))]);
        assert_eq!(from_translations(&t, |d| d.overview.as_ref()), None);
    }

    fn release_dates() -> dto::ReleaseDatesDto {
        dto::ReleaseDatesDto {
            results: vec![
                dto::ReleaseDateCountryDto {
                    iso_3166_1: "US".to_owned(),
                    release_dates: vec![dto::ReleaseDateEntryDto {
                        kind: TYPE_DIGITAL,
                        release_date: Some(date!(2024 - 04 - 16)),
                    }],
                },
                dto::ReleaseDateCountryDto {
                    iso_3166_1: "BR".to_owned(),
                    release_dates: vec![
                        dto::ReleaseDateEntryDto {
                            kind: TYPE_THEATRICAL,
                            release_date: Some(date!(2024 - 02 - 29)),
                        },
                        dto::ReleaseDateEntryDto {
                            kind: TYPE_DIGITAL,
                            release_date: Some(date!(2024 - 05 - 02)),
                        },
                    ],
                },
            ],
        }
    }

    #[test]
    fn preferred_country_wins_for_release_dates() {
        assert_eq!(
            release_date_of(&release_dates(), "BR", TYPE_DIGITAL),
            Some(date!(2024 - 05 - 02))
        );
    }

    #[test]
    fn falls_back_to_us_and_then_stops() {
        // No FR block at all → US.
        assert_eq!(
            release_date_of(&release_dates(), "FR", TYPE_DIGITAL),
            Some(date!(2024 - 04 - 16))
        );
        // Physical exists nowhere.
        assert_eq!(release_date_of(&release_dates(), "BR", TYPE_PHYSICAL), None);
    }

    #[test]
    fn an_unrelated_market_is_not_used_as_a_fallback() {
        // The shape The Matrix actually has: a digital date exists, but
        // only in a market that says nothing about availability here.
        let far_away = dto::ReleaseDatesDto {
            results: vec![dto::ReleaseDateCountryDto {
                iso_3166_1: "AE".to_owned(),
                release_dates: vec![dto::ReleaseDateEntryDto {
                    kind: TYPE_DIGITAL,
                    release_date: Some(date!(2016 - 01 - 07)),
                }],
            }],
        };
        assert_eq!(release_date_of(&far_away, "BR", TYPE_DIGITAL), None);
    }

    #[test]
    fn seasons_come_back_sorted() {
        let dto = dto::TvDetailsDto {
            id: 1,
            name: "S".to_owned(),
            original_name: None,
            overview: None,
            poster_path: None,
            backdrop_path: None,
            first_air_date: None,
            status: None,
            in_production: true,
            episode_run_time: vec![],
            next_episode_to_air: None,
            seasons: vec![
                dto::SeasonSummaryDto {
                    season_number: 2,
                    episode_count: 8,
                    air_date: None,
                },
                dto::SeasonSummaryDto {
                    season_number: 0,
                    episode_count: 3,
                    air_date: None,
                },
                dto::SeasonSummaryDto {
                    season_number: 1,
                    episode_count: 8,
                    air_date: None,
                },
            ],
            external_ids: dto::ExternalIdsDto::default(),
            translations: dto::TranslationsDto::default(),
        };
        let details = TvDetails::from_dto(dto);
        let numbers: Vec<i32> = details.seasons.iter().map(|s| s.season_number).collect();
        assert_eq!(numbers, vec![0, 1, 2], "specials sort first, as season 0");
    }
}
