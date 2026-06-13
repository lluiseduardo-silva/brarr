//! Convert a [`RawItem`] (XML-shaped) into a [`Release`] +
//! [`ReleaseEnrichment`]. Lossy on purpose — Newznab attrs carry more
//! detail than `Release` needs.

use brarr_core::{
    ExternalIds, ImdbId, Language, MalId, OffsetDateTime, Release, ReleaseEnrichment, ReleaseError,
    ReleaseKind, ReleaseUrls, Resolution, TmdbId, TrackerSource, TvdbId,
};
use time::format_description::well_known::Rfc2822;
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
    release.published_at = pick_published_at(item);
    // Newznab não traz MediaInfo, então as tags saem só do título.
    release.tags = brarr_core::parse_release_tags(&item.title);
    Ok(release)
}

/// Pick a publication timestamp for the release.
///
/// Preference order — most reliable first:
///   1. `<newznab:attr name="usenetdate">` — Unix seconds, set by the
///      indexer when the actual Usenet post was made. Newznab spec field
///      and the canonical source for real "age" in Sonarr/Radarr.
///   2. `<pubDate>` RFC 2822 element — set by every indexer but some
///      lazily emit `now()`, which kills the age signal.
///
/// Returns `None` when neither parses. The Torznab outbound feed falls
/// back to `now()` in that case (same behaviour as before).
fn pick_published_at(item: &RawItem) -> Option<OffsetDateTime> {
    if let Some(raw) = item.attr("usenetdate") {
        if let Some(ts) = parse_pubdate_value(raw) {
            return Some(ts);
        }
    }
    if let Some(raw) = item.pub_date.as_deref() {
        if let Some(ts) = parse_pubdate_value(raw) {
            return Some(ts);
        }
    }
    None
}

/// Accept either Unix seconds (`usenetdate` shape on most indexers) or
/// an RFC 2822 string (`pubDate` shape).
fn parse_pubdate_value(raw: &str) -> Option<OffsetDateTime> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(seconds) = trimmed.parse::<i64>() {
        if let Ok(ts) = OffsetDateTime::from_unix_timestamp(seconds) {
            return Some(ts);
        }
    }
    OffsetDateTime::parse(trimmed, &Rfc2822).ok()
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
    // Newznab indexers (with `extended=1`) routinely emit one
    // `<newznab:attr name="language">` per audio track instead of
    // packing them into a single comma-joined value. Iterate ALL
    // entries — `attr()` (singular) would drop everything after the
    // first. The `audio` attr is treated identically.
    for raw in item.attrs("language") {
        for piece in split_langs(raw) {
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
    // Title heuristic fallback. Newznab indexers — NZBGeek especially
    // — frequently omit the `<newznab:attr name="audio|subs|language">`
    // tags, leaving brarr with an empty enrichment that the rules
    // engine then treats as "unknown PT presence". Scene groups
    // routinely encode the language directly in the release name
    // (`*.PT-BR.*`, `*.PORTBR.*`, `*.Dublado.*`, `*.Multi5.*`), so we
    // mine the title for those tokens and additively push them into
    // the audio list. This is intentionally conservative — we only
    // recognize markers with unambiguous meaning, never speculating
    // about which languages a generic `MULTi` tag covers.
    for hint in lang_hints_from_title(&item.title) {
        push_unique(&mut audio, hint);
    }

    let lc = item.title.to_ascii_lowercase();
    let has_hdr = lc.contains("hdr")
        || lc.contains("dolby.vision")
        || lc.contains("dolby vision")
        || lc.contains(".dv.")
        || lc.contains(" dv ");

    ReleaseEnrichment {
        container_format: None,
        duration: None,
        audio_languages: audio,
        subtitle_languages: subs,
        has_forced_subs: false,
        has_hdr,
        // Newznab não traz MediaInfo; codec/bit-depth ficam por conta do
        // tokenizador de título (ver `item_to_release`).
        video_codec: None,
        video_bit_depth: None,
    }
}

/// Mine a release title for language hints scene groups embed in the
/// name. Returns a deduplicated list of [`Language`] values, additive
/// to whatever the `<newznab:attr>` block already supplied.
///
/// Recognized markers (case-insensitive, separator-agnostic — `.`, `-`,
/// `_`, and space all normalize to one whitespace before matching):
/// - PT-BR: `pt br`, `ptbr`, `portbr`, `brazilian portuguese`,
///   `dublado`, `dual.audio.*portu*` heuristic
/// - PT-PT: `pt pt`, `ptpt`, `european portuguese`
/// - PT (ambiguous): bare `portuguese` token when no region marker
///   resolved above
/// - EN: `english`, ` eng `, or a standalone `en` token
///
/// **Deliberately omitted**: `MULTi[N]?`, `DUAL`, and bare-language
/// codes like `iTA`/`FRE`/`GER`. Those tell us "this release has
/// multiple audio tracks" but not which ones — pushing arbitrary
/// languages would mislead the rules engine. The scoring layer can
/// treat the absence of a PT marker as ambiguous and rank accordingly.
fn lang_hints_from_title(title: &str) -> Vec<Language> {
    let lc = title.to_ascii_lowercase();
    let norm: String = lc
        .chars()
        .map(|c| match c {
            '.' | '-' | '_' | ',' | '[' | ']' | '(' | ')' => ' ',
            _ => c,
        })
        .collect();
    // Pad with spaces so " eng " / " en " token matchers don't miss
    // markers at the start or end of the title.
    let padded = format!(" {norm} ");
    let mut out: Vec<Language> = Vec::new();

    let pt_br_markers = [
        " pt br ",
        " ptbr ",
        " portbr ",
        " brazilian portuguese ",
        " portuguese brazilian ",
        " dublado ",
    ];
    if pt_br_markers.iter().any(|m| padded.contains(m)) {
        push_unique(&mut out, Language::PtBr);
    }

    let pt_pt_markers = [
        " pt pt ",
        " ptpt ",
        " european portuguese ",
        " portuguese european ",
        " portuguese portugal ",
    ];
    if pt_pt_markers.iter().any(|m| padded.contains(m)) {
        push_unique(&mut out, Language::PtPt);
    }

    // Ambiguous bare-Portuguese markers — only added when no region
    // marker already classified the release. ` por ` is the 3-letter
    // ISO-639-2 / scene-tag code for Portuguese and appears in
    // multi-audio release names like
    // `Movie.1999.Eng.Fre.Ger.Ita.Por.Spa.2160p.BluRay-CREW`. It can't
    // distinguish BR vs PT on its own, so we drop the ambiguous variant.
    if !out.iter().any(Language::is_portuguese)
        && (padded.contains(" portuguese ") || padded.contains(" por "))
    {
        push_unique(&mut out, Language::Pt);
    }

    // English markers. ` en ` is intentionally narrow — matching just
    // "en" as a substring would false-positive on every release name
    // (`encoded`, `seven`, etc.). The 3-letter ` eng ` scene-tag code
    // is fine because the token boundaries make it unambiguous.
    if padded.contains(" english ") || padded.contains(" eng ") || padded.contains(" en ") {
        push_unique(&mut out, Language::En);
    }

    out
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
    fn title_hint_detects_ptbr_dot_separated() {
        let hints = lang_hints_from_title("Filme.2024.1080p.WEB-DL.PT-BR-GRUPO");
        assert!(hints.contains(&Language::PtBr), "hints = {hints:?}");
    }

    #[test]
    fn title_hint_detects_dublado_as_ptbr() {
        let hints = lang_hints_from_title("Algum.Filme.2024.Dublado.1080p.WEB-DL-x264");
        assert!(hints.contains(&Language::PtBr));
    }

    #[test]
    fn title_hint_detects_portbr_tag() {
        let hints = lang_hints_from_title("Foo.Bar.2024.2160p.UHD.BluRay.PORTBR-CREW");
        assert!(hints.contains(&Language::PtBr));
    }

    #[test]
    fn title_hint_detects_pt_pt_european() {
        let hints = lang_hints_from_title("Algo.2024.1080p.European.Portuguese-GRP");
        assert!(hints.contains(&Language::PtPt));
    }

    #[test]
    fn title_hint_resolves_to_ambiguous_pt_when_no_region() {
        let hints = lang_hints_from_title("Some.Movie.2024.Portuguese.1080p");
        assert!(hints.contains(&Language::Pt));
        // Must NOT promote to PtBr/PtPt without a region marker.
        assert!(!hints.contains(&Language::PtBr));
        assert!(!hints.contains(&Language::PtPt));
    }

    #[test]
    fn title_hint_skips_multi_and_dual_intentionally() {
        // `MULTi4` and `DUAL` are too ambiguous — they tell us there's
        // more than one audio track, not which languages. The scoring
        // engine must treat the absence of a PT hint as "unknown PT",
        // not assume one is present.
        let multi = lang_hints_from_title(
            "The.Matrix.1999.2160p.HDR.UHD.BluRay.AV1.DDP5.1.Atmos.Multi4-dAV1nci",
        );
        assert!(
            multi.is_empty(),
            "multi should yield nothing, got {multi:?}"
        );
        let dual = lang_hints_from_title("Movie.2024.1080p.WEB-DL.DUAL.H264-GRP");
        assert!(dual.is_empty(), "dual should yield nothing, got {dual:?}");
    }

    #[test]
    fn title_hint_does_not_false_positive_on_encoded_or_seven() {
        // The narrow ` en ` matcher must not trip on words that
        // happen to contain `en` as a substring.
        let h = lang_hints_from_title("Seven.1995.Encoded.1080p.x265-CREW");
        assert!(!h.contains(&Language::En), "false-positive En: {h:?}");
    }

    #[test]
    fn title_hint_detects_explicit_english_token() {
        let h = lang_hints_from_title("Movie.2024.English.1080p.WEB-DL");
        assert!(h.contains(&Language::En));
    }

    #[test]
    fn dolby_vision_in_title_sets_has_hdr() {
        let body = r#"<?xml version="1.0"?>
<rss xmlns:newznab="http://www.newznab.com/DTD/2010/feeds/attributes/">
  <channel>
    <item>
      <title>Movie.2024.2160p.Dolby.Vision.WEB-DL-CREW</title>
      <guid>g</guid>
      <enclosure url="https://api.example/a?id=1" length="1"/>
    </item>
  </channel>
</rss>"#;
        let feed = parse_feed(body).unwrap();
        let r = item_to_release(&feed.items[0], tracker()).unwrap();
        assert!(r.enrichment.as_ref().unwrap().has_hdr);
    }

    #[test]
    fn repeated_language_attrs_all_land_in_audio() {
        // Newznab `extended=1` responses emit one
        // `<newznab:attr name="language">` per audio track instead of
        // one comma-joined value. The converter must walk all of them,
        // not just the first.
        let body = r#"<?xml version="1.0"?>
<rss xmlns:newznab="http://www.newznab.com/DTD/2010/feeds/attributes/">
  <channel>
    <item>
      <title>Baby.Driver.[2017].br.remux.multi.avc-d3g</title>
      <guid>4d2e</guid>
      <enclosure url="https://api.example/a?id=4d2e" length="33637273000"/>
      <newznab:attr name="language" value="English"/>
      <newznab:attr name="language" value="Italian"/>
      <newznab:attr name="language" value="Portuguese"/>
      <newznab:attr name="subs" value="English"/>
      <newznab:attr name="subs" value="Italian"/>
      <newznab:attr name="subs" value="Portuguese"/>
    </item>
  </channel>
</rss>"#;
        let feed = parse_feed(body).unwrap();
        let r = item_to_release(&feed.items[0], tracker()).unwrap();
        let e = r.enrichment.as_ref().unwrap();
        assert!(
            e.audio_languages.contains(&Language::En),
            "audio = {:?}",
            e.audio_languages
        );
        // "Portuguese" (no region) → Pt (ambiguous). Brarr's rules
        // engine treats it as PT-present-but-region-unknown.
        assert!(
            e.audio_languages.contains(&Language::Pt),
            "audio = {:?}",
            e.audio_languages
        );
        // Italian falls through to Other — preserved verbatim.
        assert!(
            e.audio_languages
                .iter()
                .any(|l| matches!(l, Language::Other(s) if s.eq_ignore_ascii_case("Italian"))),
            "audio = {:?}",
            e.audio_languages,
        );
        assert!(e.subtitle_languages.contains(&Language::En));
        assert!(e.subtitle_languages.contains(&Language::Pt));
    }

    #[test]
    fn title_hint_detects_scene_3letter_por_code() {
        // Scene release names list audio tracks as 3-letter codes
        // separated by dots:
        // `Movie.1999.Eng.Fre.Ger.Ita.Por.Spa.2160p.BluRay-CREW`.
        // The indexer may publish only `language=English` in the
        // attrs, so brarr has to mine the title for the `Por` token
        // to register PT presence at all.
        let hints = lang_hints_from_title(
            "The.Matrix.1999.Eng.Fre.Ger.Ita.Por.Spa.Cze.Hun.Pol.Rus.Tha.Tur.Jpn.2160p.BluRay.Remux.DV.HDR.HEVC.Atmos-SGF",
        );
        assert!(
            hints.contains(&Language::Pt),
            "expected Pt from `Por` token, got {hints:?}",
        );
        // ` Eng ` token must also light up English.
        assert!(hints.contains(&Language::En));
    }

    #[test]
    fn title_hint_does_not_false_positive_por_inside_porto() {
        // Plain "Porto" / "Portland" / etc. must not trip the ` por `
        // matcher — only a standalone `Por` token surrounded by
        // separators counts.
        assert!(!lang_hints_from_title("Porto.2024.1080p.WEB-DL").contains(&Language::Pt));
        assert!(!lang_hints_from_title("Portland.2024.1080p.WEB-DL").contains(&Language::Pt));
    }

    #[test]
    fn ptbr_from_title_fills_in_when_attrs_are_empty() {
        // Reproduces the NZBGeek-style scenario: indexer ships zero
        // audio/subs/language attrs, brarr must still surface PT-BR
        // when the title encodes it.
        let body = r#"<?xml version="1.0"?>
<rss xmlns:newznab="http://www.newznab.com/DTD/2010/feeds/attributes/">
  <channel>
    <item>
      <title>Filme.Br.2024.1080p.WEB-DL.PT-BR.H264-GRP</title>
      <guid>g</guid>
      <enclosure url="https://api.example/a?id=1" length="1"/>
    </item>
  </channel>
</rss>"#;
        let feed = parse_feed(body).unwrap();
        let r = item_to_release(&feed.items[0], tracker()).unwrap();
        let e = r.enrichment.as_ref().unwrap();
        assert!(
            e.audio_languages.contains(&Language::PtBr),
            "audio = {:?}",
            e.audio_languages
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
    fn published_at_picks_usenetdate_unix_seconds() {
        // NZBGeek and most Newznab indexers ship `usenetdate` as a
        // string-encoded Unix timestamp. The Torznab outbound feed
        // surfaces this as `<pubDate>` so Sonarr/Radarr can compute
        // real release age.
        let body = r#"<?xml version="1.0"?>
<rss xmlns:newznab="http://www.newznab.com/DTD/2010/feeds/attributes/">
  <channel>
    <item>
      <title>x</title>
      <guid>g</guid>
      <enclosure url="https://api.example/a?id=1" length="1"/>
      <newznab:attr name="usenetdate" value="1700000000"/>
    </item>
  </channel>
</rss>"#;
        let feed = parse_feed(body).unwrap();
        let r = item_to_release(&feed.items[0], tracker()).unwrap();
        let ts = r.published_at.unwrap();
        assert_eq!(ts.unix_timestamp(), 1_700_000_000);
    }

    #[test]
    fn published_at_falls_back_to_pubdate_rfc2822() {
        // Indexers without a `usenetdate` attr still emit the RSS
        // `<pubDate>` per item. Convert.rs should fall back to it.
        let body = r#"<?xml version="1.0"?>
<rss xmlns:newznab="http://www.newznab.com/DTD/2010/feeds/attributes/">
  <channel>
    <item>
      <title>x</title>
      <guid>g</guid>
      <pubDate>Wed, 15 Nov 2023 12:34:56 +0000</pubDate>
      <enclosure url="https://api.example/a?id=1" length="1"/>
    </item>
  </channel>
</rss>"#;
        let feed = parse_feed(body).unwrap();
        let r = item_to_release(&feed.items[0], tracker()).unwrap();
        let ts = r.published_at.unwrap();
        // 2023-11-15 12:34:56 UTC = 1700051696
        assert_eq!(ts.unix_timestamp(), 1_700_051_696);
    }

    #[test]
    fn published_at_is_none_when_provider_omits_both() {
        let body = r#"<?xml version="1.0"?>
<rss xmlns:newznab="http://www.newznab.com/DTD/2010/feeds/attributes/">
  <channel>
    <item>
      <title>x</title>
      <guid>g</guid>
      <enclosure url="https://api.example/a?id=1" length="1"/>
    </item>
  </channel>
</rss>"#;
        let feed = parse_feed(body).unwrap();
        let r = item_to_release(&feed.items[0], tracker()).unwrap();
        assert!(r.published_at.is_none());
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
