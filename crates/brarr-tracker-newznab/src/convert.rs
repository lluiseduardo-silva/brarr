//! Convert a [`RawItem`] (XML-shaped) into a [`Release`] +
//! [`ReleaseEnrichment`]. Lossy on purpose — Newznab attrs carry more
//! detail than `Release` needs.

use brarr_core::{
    ExternalIds, ImdbId, Language, MalId, Release, ReleaseEnrichment, ReleaseError, ReleaseKind,
    ReleaseUrls, Resolution, TmdbId, TrackerSource, TvdbId,
};
use url::Url;

use crate::dto::RawItem;

/// Convert one Newznab item into a `Release`. `tracker` is supplied by
/// the host (the indexer doesn't repeat its own name), and the IDs +
/// enrichment are pulled from the `<newznab:attr>` block.
///
/// # Errors
///
/// Returns the underlying [`ReleaseError`] when invariants fail
/// (empty title, empty id). The caller skips bad items and keeps
/// going — one malformed item doesn't abort the whole search.
pub fn item_to_release(item: &RawItem, tracker: TrackerSource) -> Result<Release, ReleaseError> {
    let id = pick_release_id(item);
    let resolution = guess_resolution(&item.title);
    let kind = guess_kind(&item.title);
    let mut release = Release::new(&id, tracker, &item.title, kind, resolution, item.size_bytes)?;
    release.snatches = item.attr_u32("grabs").unwrap_or(0);
    release.external_ids = pick_external_ids(item);
    release.urls = pick_urls(item);
    release.enrichment = Some(build_enrichment(item));
    Ok(release)
}

/// Newznab `guid` is usually a URL or opaque hash. We prefer something
/// short + stable: the indexer-side id derived from the download URL
/// query string when possible, falling back to the guid verbatim, then
/// to the title (last-resort, ugly but legal).
fn pick_release_id(item: &RawItem) -> String {
    if let Some(url) = item.download_url.as_deref()
        && let Ok(parsed) = Url::parse(url)
        && let Some((_, id)) = parsed.query_pairs().find(|(k, _)| k == "id")
    {
        return id.into_owned();
    }
    if !item.guid.is_empty() {
        return item.guid.clone();
    }
    item.title.clone()
}

fn pick_external_ids(item: &RawItem) -> ExternalIds {
    let mut ids = ExternalIds::default();
    ids.tmdb = item.attr_u32("tmdb").and_then(|v| TmdbId::new(v).ok());
    // `imdb` is usually a numeric tt-id without the "tt" prefix; some
    // indexers prefix it. Strip leading "tt" before parsing.
    ids.imdb = item
        .attr("imdb")
        .map(|s| s.trim_start_matches("tt"))
        .and_then(|s| s.parse::<u32>().ok())
        .and_then(|v| ImdbId::new(v).ok());
    ids.tvdb = item
        .attr_u32("tvdbid")
        .or_else(|| item.attr_u32("thetvdbid"))
        .and_then(|v| TvdbId::new(v).ok());
    // MalId is uncommon in Newznab feeds but cheap to honor.
    ids.mal = item.attr_u32("mal").and_then(|v| MalId::new(v).ok());
    ids
}

fn pick_urls(item: &RawItem) -> ReleaseUrls {
    let mut urls = ReleaseUrls::default();
    urls.download = item
        .download_url
        .as_deref()
        .and_then(|s| Url::parse(s).ok());
    urls.details = item.details_url.as_deref().and_then(|s| Url::parse(s).ok());
    urls
}

fn build_enrichment(item: &RawItem) -> ReleaseEnrichment {
    let mut audio = Vec::new();
    // Single `language` attr is often the primary audio when no `audio`
    // attrs exist (NZBGeek behaviour). Treat both as additive.
    if let Some(lang) = item.attr("language") {
        for piece in split_langs(lang) {
            push_unique(&mut audio, piece);
        }
    }
    for raw in item.attrs("audio") {
        for piece in split_langs(raw) {
            push_unique(&mut audio, piece);
        }
    }
    let mut subs = Vec::new();
    for raw in item.attrs("subs") {
        for piece in split_langs(raw) {
            push_unique(&mut subs, piece);
        }
    }
    let has_hdr = item.title.to_ascii_lowercase().contains("hdr");

    ReleaseEnrichment {
        container_format: None,
        duration: None,
        audio_languages: audio,
        subtitle_languages: subs,
        has_forced_subs: false,
        has_hdr,
    }
}

fn push_unique(out: &mut Vec<Language>, lang: Language) {
    if !out.contains(&lang) {
        out.push(lang);
    }
}

/// Split a free-form language attr value into [`Language`] entries.
/// Newznab feeds use comma-, semicolon-, or pipe-separated lists.
/// Each piece routes through `Language::from_mediainfo` with no title
/// hint — fine for the common case (`"Portuguese (BR)"`, `"English"`);
/// the user-facing edge of "Portuguese" alone still resolves to
/// `Language::Pt`, which the rules engine treats as ambiguous PT.
fn split_langs(raw: &str) -> impl Iterator<Item = Language> + '_ {
    raw.split([',', ';', '|'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| Language::from_mediainfo(s, None))
}

fn guess_resolution(title: &str) -> Resolution {
    let t = title.to_ascii_lowercase();
    if t.contains("2160p") || t.contains("4k") {
        Resolution::P2160
    } else if t.contains("1080p") {
        Resolution::P1080
    } else if t.contains("720p") {
        Resolution::P720
    } else if t.contains("480p") || t.contains(".sd.") || t.contains(" sd ") {
        Resolution::Sd
    } else {
        Resolution::Other("unknown".into())
    }
}

fn guess_kind(title: &str) -> ReleaseKind {
    let t = title.to_ascii_lowercase();
    if t.contains("web-dl") || t.contains("webdl") || t.contains(".web.") {
        ReleaseKind::WebDl
    } else if t.contains("remux") || t.contains("bluray") || t.contains("blu-ray") {
        ReleaseKind::BluRay
    } else if t.contains("hdtv") {
        ReleaseKind::HdTv
    } else if t.contains("dvd") {
        ReleaseKind::Dvd
    } else {
        ReleaseKind::Other("unknown".into())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::dto::parse_feed;

    fn tracker() -> TrackerSource {
        TrackerSource::new("nzbgeek", Url::parse("https://api.example/").unwrap()).unwrap()
    }

    const FEED: &str = r#"<?xml version="1.0"?>
<rss xmlns:newznab="http://www.newznab.com/DTD/2010/feeds/attributes/">
  <channel>
    <item>
      <title>The.Matrix.1999.2160p.UHD.BluRay.HDR.x265-FOO</title>
      <guid>abc</guid>
      <enclosure url="https://api.example/api?t=get&amp;id=xyz789" length="9608016733" type="application/x-nzb"/>
      <newznab:attr name="grabs" value="42"/>
      <newznab:attr name="imdb" value="9999001"/>
      <newznab:attr name="tmdb" value="603"/>
      <newznab:attr name="language" value="English"/>
      <newznab:attr name="audio" value="Portuguese"/>
      <newznab:attr name="subs" value="English,Portuguese (BR)"/>
    </item>
  </channel>
</rss>"#;

    #[test]
    fn converts_a_full_item() {
        let feed = parse_feed(FEED).unwrap();
        let release = item_to_release(&feed.items[0], tracker()).unwrap();
        assert_eq!(
            release.title,
            "The.Matrix.1999.2160p.UHD.BluRay.HDR.x265-FOO"
        );
        assert_eq!(release.tracker_release_id, "xyz789");
        assert_eq!(release.size_bytes, 9_608_016_733);
        assert_eq!(release.snatches, 42);
        assert!(matches!(release.resolution, Resolution::P2160));
        assert!(matches!(release.kind, ReleaseKind::BluRay));
        assert_eq!(release.external_ids.tmdb.map(TmdbId::get), Some(603));
        assert_eq!(release.external_ids.imdb.map(ImdbId::get), Some(9_999_001));
        let e = release.enrichment.as_ref().unwrap();
        assert!(e.has_hdr);
        assert!(e.audio_languages.contains(&Language::En));
        assert!(
            e.audio_languages.contains(&Language::PtPt)
                || e.audio_languages.contains(&Language::Pt)
        );
        assert!(
            e.subtitle_languages
                .iter()
                .any(|l| matches!(l, Language::En))
        );
        assert!(
            e.subtitle_languages
                .iter()
                .any(|l| matches!(l, Language::PtBr | Language::Pt))
        );
    }

    #[test]
    fn imdb_with_tt_prefix_is_stripped() {
        let body = r#"<?xml version="1.0"?>
<rss xmlns:newznab="http://www.newznab.com/DTD/2010/feeds/attributes/">
  <channel>
    <item>
      <title>x</title>
      <guid>g</guid>
      <enclosure url="https://api.example/a?id=1" length="1"/>
      <newznab:attr name="imdb" value="tt9999001"/>
    </item>
  </channel>
</rss>"#;
        let feed = parse_feed(body).unwrap();
        let r = item_to_release(&feed.items[0], tracker()).unwrap();
        assert_eq!(r.external_ids.imdb.map(ImdbId::get), Some(9_999_001));
    }

    #[test]
    fn empty_title_rejected() {
        let body = r#"<?xml version="1.0"?>
<rss xmlns:newznab="http://www.newznab.com/DTD/2010/feeds/attributes/">
  <channel>
    <item>
      <title></title>
      <guid>g</guid>
      <enclosure url="https://api.example/a?id=1" length="1"/>
    </item>
  </channel>
</rss>"#;
        let feed = parse_feed(body).unwrap();
        let err = item_to_release(&feed.items[0], tracker()).unwrap_err();
        assert!(format!("{err}").to_ascii_lowercase().contains("title"));
    }
}
