//! What the rest of brarr sees.
//!
//! The conversions from [`crate::dto`] live here so the upstream's
//! looseness is absorbed at one boundary rather than leaking into the
//! orchestrator — the same split [`brarr-tmdb`](../brarr_tmdb/index.html)
//! makes, for the same reason.

use time::{Date, OffsetDateTime, Time, UtcOffset, macros::format_description};

use crate::dto;

/// Which numbering to ask for.
///
/// **The reason this crate exists.** The same episodes come back under
/// different coordinates depending on the season type, and the split the
/// scene uses is the one `TheTVDB` calls *official* — Dragon Ball Super is
/// five seasons here and one of 131 on TMDB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeasonType {
    /// Whatever the series is set to by default. Usually `Official`.
    Default,
    /// Aired order, split into seasons the way the broadcaster did.
    Official,
    /// DVD ordering.
    Dvd,
    /// One season, numbered straight through. What anime releases use
    /// when they carry no `SxxEyy` at all.
    Absolute,
    /// A contributor-defined alternate ordering.
    Alternate,
    /// A region-specific ordering.
    Regional,
}

impl SeasonType {
    /// Path segment for `/series/{id}/episodes/{season-type}`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Official => "official",
            Self::Dvd => "dvd",
            Self::Absolute => "absolute",
            Self::Alternate => "alternate",
            Self::Regional => "regional",
        }
    }
}

/// One episode, under the season type that was requested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Episode {
    /// `TheTVDB`'s own episode id. **Stable across season types**, which
    /// is what makes two calls joinable into a translation table.
    pub id: i64,
    /// Episode title, when a contributor has filled one in.
    pub name: Option<String>,
    /// Season under the requested season type.
    pub season_number: i32,
    /// Number within that season, under the requested season type.
    pub number: i32,
    /// Position in the series as a whole, when `TheTVDB` has one.
    pub absolute_number: Option<i32>,
    /// First air date, midnight UTC. Absent until scheduled.
    pub aired: Option<OffsetDateTime>,
}

/// A series' episodes under one season type.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SeriesEpisodes {
    /// `TheTVDB` series id.
    pub series_id: Option<i64>,
    /// Series name, for logs and for the panel.
    pub series_name: Option<String>,
    /// Every episode, in the order the API returned them.
    pub episodes: Vec<Episode>,
}

impl Episode {
    /// Build from the wire shape. `None` when the record carries no
    /// usable coordinates — an episode with no season and no number
    /// cannot be joined to anything, and inventing a zero for it would
    /// put it in TMDB's specials bucket.
    ///
    /// **The id is required for the same reason.** It is the identity
    /// that survives a change of season type, so an episode without one
    /// cannot be joined across the two calls this crate exists to make;
    /// it is also what deduplicates a paginated walk, and a default of
    /// zero would silently collapse every such episode onto one key.
    pub(crate) fn from_dto(dto: &dto::EpisodeDto) -> Option<Self> {
        let id = dto.id.filter(|v| *v > 0)?;
        let season = i32::try_from(dto.season_number?).ok()?;
        let number = i32::try_from(dto.number?).ok()?;
        Some(Self {
            id,
            name: dto.name.clone().filter(|n| !n.trim().is_empty()),
            season_number: season,
            number,
            absolute_number: dto
                .absolute_number
                .and_then(|v| i32::try_from(v).ok())
                .filter(|v| *v > 0),
            aired: dto.aired.as_deref().and_then(parse_date),
        })
    }
}

/// `YYYY-MM-DD` at midnight UTC.
///
/// `TheTVDB` carries a date with no time and no zone. Midnight UTC is the
/// same convention `brarr-tmdb` uses, so the two sources produce
/// comparable values and `coverage`'s "has it aired" question does not
/// depend on which one filled the column.
fn parse_date(raw: &str) -> Option<OffsetDateTime> {
    let format = format_description!("[year]-[month]-[day]");
    Date::parse(raw.trim(), &format)
        .ok()
        .map(|d| OffsetDateTime::new_in_offset(d, Time::MIDNIGHT, UtcOffset::UTC))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn dto(season: Option<i64>, number: Option<i64>, absolute: Option<i64>) -> dto::EpisodeDto {
        serde_json::from_value(serde_json::json!({
            "id": 42,
            "name": "Um",
            "seasonNumber": season,
            "number": number,
            "absoluteNumber": absolute,
            "aired": "2016-01-17",
        }))
        .unwrap()
    }

    #[test]
    fn an_episode_carries_both_axes() {
        let episode = Episode::from_dto(&dto(Some(2), Some(1), Some(15))).unwrap();
        assert_eq!(episode.season_number, 2);
        assert_eq!(episode.number, 1);
        assert_eq!(episode.absolute_number, Some(15));
        assert_eq!(episode.aired, Some(datetime!(2016-01-17 0:00 UTC)));
    }

    /// `TheTVDB` is community-edited: a blank absolute number is normal and
    /// must not cost the episode its row.
    #[test]
    fn a_missing_absolute_number_is_not_fatal() {
        let episode = Episode::from_dto(&dto(Some(1), Some(3), None)).unwrap();
        assert_eq!(episode.absolute_number, None);
        // Zero is how the API spells "unset"; it is not episode zero.
        let zeroed = Episode::from_dto(&dto(Some(1), Some(3), Some(0))).unwrap();
        assert_eq!(zeroed.absolute_number, None);
    }

    /// No coordinates means nothing to join against. Refused rather than
    /// defaulted — a zero would land it in the specials bucket.
    #[test]
    fn an_episode_without_coordinates_is_refused() {
        assert!(Episode::from_dto(&dto(None, Some(1), None)).is_none());
        assert!(Episode::from_dto(&dto(Some(1), None, None)).is_none());
    }

    /// The id is the identity that survives a change of season type, so
    /// an episode without one cannot be joined across the two calls this
    /// crate exists to make — and it is what deduplicates the paginated
    /// walk, where a default of zero would collapse every such episode
    /// onto one key.
    #[test]
    fn an_episode_without_an_id_is_refused() {
        let mut raw = dto(Some(1), Some(1), None);
        raw.id = None;
        assert!(Episode::from_dto(&raw).is_none());
        raw.id = Some(0);
        assert!(Episode::from_dto(&raw).is_none());
    }

    #[test]
    fn season_types_spell_their_path_segment() {
        assert_eq!(SeasonType::Official.as_str(), "official");
        assert_eq!(SeasonType::Absolute.as_str(), "absolute");
        assert_eq!(SeasonType::Default.as_str(), "default");
    }

    #[test]
    fn an_unscheduled_episode_has_no_air_date() {
        assert_eq!(parse_date(""), None);
        assert_eq!(parse_date("0000-00-00"), None);
        assert_eq!(
            parse_date("2016-01-17"),
            Some(datetime!(2016-01-17 0:00 UTC))
        );
    }
}
