//! Plugin-facing data types.
//!
//! These DTOs mirror a *subset* of [`brarr_core::Release`] — plugins
//! produce them, the host converts them. Keeping a dedicated schema
//! decouples the plugin ABI from internal type evolution: we can
//! refactor `Release` freely as long as `From<PluginRelease> for Release`
//! still type-checks.
//!
//! The conversion deliberately discards or normalizes anything the
//! plugin must not be trusted with — e.g. `Release::tracker` is filled
//! from the plugin's display name on the host side, not from the
//! plugin's output, so a misbehaving plugin can't impersonate a
//! different tracker.

use brarr_core::{
    ExternalIds, ImdbId, MalId, Release, ReleaseError, ReleaseKind, ReleaseUrls, Resolution,
    TmdbId, TrackerSource, TvdbId,
};
use serde::{Deserialize, Serialize};

/// JSON shape a plugin returns from `plugin_search_by_tmdb`.
///
/// All fields except `id`, `title`, and `size_bytes` are optional. Bad
/// values (e.g. negative size, empty title) cause the host to skip the
/// entry while logging a warning — one malformed release does not abort
/// the whole search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginRelease {
    /// Tracker-side release id (string, typically numeric).
    pub id: String,
    /// Release title.
    pub title: String,
    /// Release kind — free-form. Maps to [`ReleaseKind::Other`] when
    /// none of the canonical labels match.
    #[serde(default)]
    pub kind: Option<String>,
    /// Resolution label (`"1080p"`, `"2160p"`, etc.).
    #[serde(default)]
    pub resolution: Option<String>,
    /// Total size in bytes.
    pub size_bytes: u64,
    /// Seeder count snapshot.
    #[serde(default)]
    pub seeders: u32,
    /// Leecher count snapshot.
    #[serde(default)]
    pub leechers: u32,
    /// Snatches count snapshot.
    #[serde(default)]
    pub snatches: u32,
    /// Release year (movies / TV).
    #[serde(default)]
    pub year: Option<u16>,
    /// External ids — at minimum `tmdb`; others optional.
    #[serde(default)]
    pub external_ids: PluginExternalIds,
    /// Tracker-side absolute URLs.
    #[serde(default)]
    pub urls: PluginUrls,
}

/// External ids subset.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginExternalIds {
    /// TMDb id (movies / TV).
    #[serde(default)]
    pub tmdb: Option<u32>,
    /// IMDb id (numeric portion of the `tt`-prefixed identifier).
    #[serde(default)]
    pub imdb: Option<u32>,
    /// TVDb id.
    #[serde(default)]
    pub tvdb: Option<u32>,
    /// MyAnimeList id.
    #[serde(default)]
    pub mal: Option<u32>,
}

/// URL subset.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginUrls {
    /// Page URL for the release.
    #[serde(default)]
    pub details: Option<String>,
    /// Download URL (`.torrent` file).
    #[serde(default)]
    pub download: Option<String>,
}

/// Convert a [`PluginRelease`] to a [`brarr_core::Release`]. The host
/// supplies the `tracker` source so the plugin cannot lie about its
/// identity.
///
/// # Errors
///
/// Returns the underlying [`ReleaseError`] if the canonical invariants
/// fail (empty id, empty title).
pub fn to_release(plugin: &PluginRelease, tracker: TrackerSource) -> Result<Release, ReleaseError> {
    let kind = plugin
        .kind
        .as_deref()
        .map_or(ReleaseKind::Other("unknown".to_string()), parse_kind);
    let resolution = plugin
        .resolution
        .as_deref()
        .map_or(Resolution::Other("unknown".to_string()), parse_resolution);

    let mut release = Release::new(
        &plugin.id,
        tracker,
        &plugin.title,
        kind,
        resolution,
        plugin.size_bytes,
    )?;
    release.seeders = plugin.seeders;
    release.leechers = plugin.leechers;
    release.snatches = plugin.snatches;
    release.year = plugin.year;
    let mut ids = ExternalIds::default();
    ids.tmdb = plugin.external_ids.tmdb.and_then(|v| TmdbId::new(v).ok());
    ids.imdb = plugin.external_ids.imdb.and_then(|v| ImdbId::new(v).ok());
    ids.tvdb = plugin.external_ids.tvdb.and_then(|v| TvdbId::new(v).ok());
    ids.mal = plugin.external_ids.mal.and_then(|v| MalId::new(v).ok());
    release.external_ids = ids;

    let mut urls = ReleaseUrls::default();
    urls.details = plugin
        .urls
        .details
        .as_deref()
        .and_then(|s| url::Url::parse(s).ok());
    urls.download = plugin
        .urls
        .download
        .as_deref()
        .and_then(|s| url::Url::parse(s).ok());
    release.urls = urls;

    Ok(release)
}

fn parse_kind(s: &str) -> ReleaseKind {
    match s {
        "WEB-DL" | "WebDl" => ReleaseKind::WebDl,
        "BluRay" | "Blu-Ray" => ReleaseKind::BluRay,
        "Encode" => ReleaseKind::Encode,
        "HDTV" => ReleaseKind::HdTv,
        "DVD" => ReleaseKind::Dvd,
        other => ReleaseKind::Other(other.to_string()),
    }
}

fn parse_resolution(s: &str) -> Resolution {
    match s {
        "SD" => Resolution::Sd,
        "720p" => Resolution::P720,
        "1080p" => Resolution::P1080,
        "2160p" => Resolution::P2160,
        other => Resolution::Other(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn tracker() -> TrackerSource {
        TrackerSource::new("test", url::Url::parse("https://t.example/").unwrap()).unwrap()
    }

    #[test]
    fn to_release_maps_canonical_fields() {
        let plugin = PluginRelease {
            id: "42".into(),
            title: "Matrix 1999".into(),
            kind: Some("BluRay".into()),
            resolution: Some("2160p".into()),
            size_bytes: 1_000,
            seeders: 10,
            leechers: 1,
            snatches: 5,
            year: Some(1999),
            external_ids: PluginExternalIds {
                tmdb: Some(603),
                imdb: Some(133_093),
                tvdb: None,
                mal: None,
            },
            urls: PluginUrls {
                details: Some("https://t.example/torrents/42".into()),
                download: None,
            },
        };
        let r = to_release(&plugin, tracker()).unwrap();
        assert_eq!(r.tracker_release_id, "42");
        assert_eq!(r.title, "Matrix 1999");
        assert!(matches!(r.kind, ReleaseKind::BluRay));
        assert!(matches!(r.resolution, Resolution::P2160));
        assert_eq!(r.size_bytes, 1_000);
        assert_eq!(r.seeders, 10);
        assert_eq!(r.year, Some(1999));
        assert_eq!(r.external_ids.tmdb.map(TmdbId::get), Some(603));
        assert_eq!(r.external_ids.imdb.map(ImdbId::get), Some(133_093));
        assert!(r.urls.details.is_some());
    }

    #[test]
    fn unknown_kind_and_resolution_fall_through_to_other() {
        let plugin = PluginRelease {
            id: "1".into(),
            title: "x".into(),
            kind: Some("ChickenWingsRip".into()),
            resolution: Some("8K".into()),
            size_bytes: 1,
            seeders: 0,
            leechers: 0,
            snatches: 0,
            year: None,
            external_ids: PluginExternalIds::default(),
            urls: PluginUrls::default(),
        };
        let r = to_release(&plugin, tracker()).unwrap();
        assert!(matches!(r.kind, ReleaseKind::Other(ref s) if s == "ChickenWingsRip"));
        assert!(matches!(r.resolution, Resolution::Other(ref s) if s == "8K"));
    }

    #[test]
    fn empty_title_rejected_by_release_invariants() {
        let plugin = PluginRelease {
            id: "1".into(),
            title: String::new(),
            kind: None,
            resolution: None,
            size_bytes: 1,
            seeders: 0,
            leechers: 0,
            snatches: 0,
            year: None,
            external_ids: PluginExternalIds::default(),
            urls: PluginUrls::default(),
        };
        let err = to_release(&plugin, tracker()).unwrap_err();
        assert!(format!("{err}").to_ascii_lowercase().contains("title"));
    }
}
