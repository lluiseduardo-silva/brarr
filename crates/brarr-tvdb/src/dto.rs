//! Wire shapes for the TVDB v4 API.
//!
//! Deliberately separate from [`crate::model`], and everything optional:
//! `TheTVDB` is community-edited, so a field the schema documents is a
//! field some contributor has left blank. A missing `absoluteNumber` on
//! one episode must not fail the other four hundred.

use serde::Deserialize;

/// Every v4 response is `{ status, data, links? }`.
#[derive(Debug, Deserialize)]
pub(crate) struct Envelope<T> {
    pub data: Option<T>,
    #[serde(default)]
    pub links: Option<Links>,
}

/// Pagination cursor. `next` is absent or null on the last page, which
/// is what ends the walk.
///
/// **Not `rename_all`, unlike every record below.** The v4 schema spells
/// record fields in `camelCase` (`seasonNumber`) and these in `snake_case`
/// (`total_items`, `page_size`) — an inconsistency in the API, not a
/// typo here, and a blanket rename would silently null both fields.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct Links {
    #[serde(default)]
    pub next: Option<String>,
    #[serde(default)]
    pub total_items: Option<i64>,
}

/// `POST /login` payload.
#[derive(Debug, Deserialize)]
pub(crate) struct TokenDto {
    pub token: Option<String>,
}

/// `GET /series/{id}/episodes/{season-type}` payload.
///
/// The series record comes back alongside the episodes, which is why the
/// endpoint is described as returning a "series base record with
/// episodes" rather than a bare list.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SeriesEpisodesDto {
    #[serde(default)]
    pub series: Option<SeriesDto>,
    #[serde(default)]
    pub episodes: Vec<EpisodeDto>,
}

/// The bits of a series record brarr reads.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SeriesDto {
    #[serde(default)]
    pub id: Option<i64>,
    #[serde(default)]
    pub name: Option<String>,
}

/// One episode, in whichever season type was asked for.
///
/// **`seasonNumber` and `number` are relative to the requested season
/// type.** Asking for `official` and asking for `absolute` return the
/// same episodes under different coordinates — which is the entire
/// reason brarr talks to this API.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EpisodeDto {
    #[serde(default)]
    pub id: Option<i64>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub season_number: Option<i64>,
    #[serde(default)]
    pub number: Option<i64>,
    #[serde(default)]
    pub absolute_number: Option<i64>,
    /// `YYYY-MM-DD`, or absent for an unscheduled episode.
    #[serde(default)]
    pub aired: Option<String>,
}

/// `GET /search/remoteid/{id}` payload entry.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RemoteIdMatchDto {
    #[serde(default)]
    pub series: Option<SeriesDto>,
}
