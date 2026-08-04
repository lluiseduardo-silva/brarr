//! Serde shapes for the TMDB v3 responses brarr consumes.
//!
//! Kept private to the crate: callers get the cleaned-up types in
//! [`crate::model`] instead. Two pieces of TMDB variance are handled
//! here rather than leaking outward:
//!
//! - Dates arrive as `"1999-03-30"`, as `""` (an *empty string*, not
//!   null) when unknown, and occasionally as a full RFC 3339 timestamp
//!   in `release_dates`. All three collapse to `Option<Date>`.
//! - Every optional string field can be either absent, `null`, or `""`.

use serde::{Deserialize, Deserializer};
use time::{
    Date, OffsetDateTime, format_description::well_known::Rfc3339, macros::format_description,
};

const YMD: &[time::format_description::FormatItem<'_>] =
    format_description!("[year]-[month]-[day]");

/// Parse a TMDB date field, tolerating `null`, `""` and full timestamps.
pub(crate) fn de_opt_date<'de, D>(de: D) -> Result<Option<Date>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw: Option<String> = Option::deserialize(de)?;
    Ok(raw.and_then(|s| parse_date(&s)))
}

/// `"1999-03-30"` or `"1999-09-21T00:00:00.000Z"` → [`Date`].
pub(crate) fn parse_date(raw: &str) -> Option<Date> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(d) = Date::parse(trimmed, YMD) {
        return Some(d);
    }
    OffsetDateTime::parse(trimmed, &Rfc3339)
        .ok()
        .map(OffsetDateTime::date)
}

/// Treat `""` as absent so downstream code never has to.
pub(crate) fn de_opt_string<'de, D>(de: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw: Option<String> = Option::deserialize(de)?;
    Ok(raw.filter(|s| !s.trim().is_empty()))
}

/// `/search/movie` and the `movie_results` of `/find`.
#[derive(Debug, Deserialize)]
pub(crate) struct MovieSummaryDto {
    pub id: i64,
    pub title: String,
    #[serde(default, deserialize_with = "de_opt_string")]
    pub original_title: Option<String>,
    #[serde(default, deserialize_with = "de_opt_string")]
    pub overview: Option<String>,
    #[serde(default, deserialize_with = "de_opt_string")]
    pub poster_path: Option<String>,
    #[serde(default, deserialize_with = "de_opt_string")]
    pub backdrop_path: Option<String>,
    #[serde(default, deserialize_with = "de_opt_date")]
    pub release_date: Option<Date>,
}

/// `/search/tv` and the `tv_results` of `/find`. TMDB names series
/// fields `name` / `original_name` / `first_air_date` rather than
/// reusing the movie names.
#[derive(Debug, Deserialize)]
pub(crate) struct TvSummaryDto {
    pub id: i64,
    pub name: String,
    #[serde(default, deserialize_with = "de_opt_string")]
    pub original_name: Option<String>,
    #[serde(default, deserialize_with = "de_opt_string")]
    pub overview: Option<String>,
    #[serde(default, deserialize_with = "de_opt_string")]
    pub poster_path: Option<String>,
    #[serde(default, deserialize_with = "de_opt_string")]
    pub backdrop_path: Option<String>,
    #[serde(default, deserialize_with = "de_opt_date")]
    pub first_air_date: Option<Date>,
}

/// The paged envelope every `/search/*` endpoint wraps its hits in.
///
/// The explicit `bound` is load-bearing: with a bare `#[serde(default)]`
/// on a generic field the derive infers `T: Default` and every DTO would
/// have to implement it for no reason.
#[derive(Debug, Deserialize)]
#[serde(bound(deserialize = "T: Deserialize<'de>"))]
pub(crate) struct PageDto<T> {
    #[serde(default = "Vec::new")]
    pub results: Vec<T>,
}

/// `/find/{external_id}`.
#[derive(Debug, Deserialize)]
pub(crate) struct FindDto {
    #[serde(default)]
    pub movie_results: Vec<MovieSummaryDto>,
    #[serde(default)]
    pub tv_results: Vec<TvSummaryDto>,
}

/// External ids, appended to a details call.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ExternalIdsDto {
    #[serde(default, deserialize_with = "de_opt_string")]
    pub imdb_id: Option<String>,
    /// Series only — TMDB has no tvdb id for movies.
    #[serde(default)]
    pub tvdb_id: Option<i64>,
}

/// One country's release-date block. `type` is the interesting field:
/// 3 = theatrical, 4 = digital, 5 = physical.
#[derive(Debug, Deserialize)]
pub(crate) struct ReleaseDateEntryDto {
    #[serde(rename = "type")]
    pub kind: i64,
    #[serde(default, deserialize_with = "de_opt_date")]
    pub release_date: Option<Date>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ReleaseDateCountryDto {
    pub iso_3166_1: String,
    #[serde(default)]
    pub release_dates: Vec<ReleaseDateEntryDto>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ReleaseDatesDto {
    #[serde(default)]
    pub results: Vec<ReleaseDateCountryDto>,
}

/// The payload of one translation entry.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct TranslationDataDto {
    #[serde(default, deserialize_with = "de_opt_string")]
    pub title: Option<String>,
    /// Series translations use `name` where movies use `title`.
    #[serde(default, deserialize_with = "de_opt_string")]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "de_opt_string")]
    pub overview: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct TranslationDto {
    pub iso_639_1: String,
    pub iso_3166_1: String,
    #[serde(default)]
    pub data: TranslationDataDto,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct TranslationsDto {
    #[serde(default)]
    pub translations: Vec<TranslationDto>,
}

/// `/movie/{id}?append_to_response=external_ids,release_dates,translations`.
#[derive(Debug, Deserialize)]
pub(crate) struct MovieDetailsDto {
    pub id: i64,
    pub title: String,
    #[serde(default, deserialize_with = "de_opt_string")]
    pub original_title: Option<String>,
    #[serde(default, deserialize_with = "de_opt_string")]
    pub overview: Option<String>,
    #[serde(default, deserialize_with = "de_opt_string")]
    pub poster_path: Option<String>,
    #[serde(default, deserialize_with = "de_opt_string")]
    pub backdrop_path: Option<String>,
    #[serde(default, deserialize_with = "de_opt_date")]
    pub release_date: Option<Date>,
    #[serde(default)]
    pub runtime: Option<i64>,
    #[serde(default, deserialize_with = "de_opt_string")]
    pub status: Option<String>,
    /// Present on the top level too, not only under `external_ids`.
    #[serde(default, deserialize_with = "de_opt_string")]
    pub imdb_id: Option<String>,
    #[serde(default)]
    pub external_ids: ExternalIdsDto,
    #[serde(default)]
    pub release_dates: ReleaseDatesDto,
    #[serde(default)]
    pub translations: TranslationsDto,
}

/// One entry of a series' `seasons` array.
#[derive(Debug, Deserialize)]
pub(crate) struct SeasonSummaryDto {
    pub season_number: i64,
    #[serde(default)]
    pub episode_count: i64,
    #[serde(default, deserialize_with = "de_opt_date")]
    pub air_date: Option<Date>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct NextEpisodeDto {
    #[serde(default, deserialize_with = "de_opt_date")]
    pub air_date: Option<Date>,
}

/// `/tv/{id}?append_to_response=external_ids,translations`.
#[derive(Debug, Deserialize)]
pub(crate) struct TvDetailsDto {
    pub id: i64,
    pub name: String,
    #[serde(default, deserialize_with = "de_opt_string")]
    pub original_name: Option<String>,
    #[serde(default, deserialize_with = "de_opt_string")]
    pub overview: Option<String>,
    #[serde(default, deserialize_with = "de_opt_string")]
    pub poster_path: Option<String>,
    #[serde(default, deserialize_with = "de_opt_string")]
    pub backdrop_path: Option<String>,
    #[serde(default, deserialize_with = "de_opt_date")]
    pub first_air_date: Option<Date>,
    #[serde(default, deserialize_with = "de_opt_string")]
    pub status: Option<String>,
    #[serde(default)]
    pub in_production: bool,
    #[serde(default)]
    pub episode_run_time: Vec<i64>,
    #[serde(default)]
    pub next_episode_to_air: Option<NextEpisodeDto>,
    #[serde(default)]
    pub seasons: Vec<SeasonSummaryDto>,
    #[serde(default)]
    pub external_ids: ExternalIdsDto,
    #[serde(default)]
    pub translations: TranslationsDto,
}

#[derive(Debug, Deserialize)]
pub(crate) struct EpisodeDto {
    pub episode_number: i64,
    pub season_number: i64,
    #[serde(default, deserialize_with = "de_opt_string")]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "de_opt_date")]
    pub air_date: Option<Date>,
}

/// `/tv/{id}/season/{n}`.
#[derive(Debug, Deserialize)]
pub(crate) struct SeasonDetailsDto {
    pub season_number: i64,
    #[serde(default, deserialize_with = "de_opt_date")]
    pub air_date: Option<Date>,
    #[serde(default)]
    pub episodes: Vec<EpisodeDto>,
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

    #[test]
    fn parses_a_plain_ymd_date() {
        assert_eq!(parse_date("1999-03-30"), Some(date!(1999 - 03 - 30)));
    }

    #[test]
    fn parses_the_rfc3339_form_used_inside_release_dates() {
        assert_eq!(
            parse_date("1999-09-21T00:00:00.000Z"),
            Some(date!(1999 - 09 - 21))
        );
    }

    #[test]
    fn empty_string_is_absent_not_an_error() {
        // TMDB returns "" rather than null for an unknown air date; a
        // strict parse here would fail the whole record.
        assert_eq!(parse_date(""), None);
        assert_eq!(parse_date("   "), None);
    }

    #[test]
    fn garbage_degrades_to_none() {
        assert_eq!(parse_date("nao e uma data"), None);
    }
}
