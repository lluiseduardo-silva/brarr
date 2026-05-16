//! DTOs that mirror the Newznab `<rss>` response surface.
//!
//! We hand-roll the parser around `quick-xml`'s pull API rather than
//! using `serde`-derived XML because Newznab's `<newznab:attr>` shape
//! (repeated elements with `name`/`value` pairs) maps cleanly to a
//! `HashMap<String, Vec<String>>` and that's friendlier than a Vec of
//! struct-with-two-fields.
//!
//! `unescape_value` is the most ergonomic way to entity-decode an
//! attribute value (turns `&amp;` into `&`, etc.). quick-xml 0.40
//! deprecated it; the suggested replacement (`normalized_value`)
//! returns bytes without entity decoding. Until quick-xml ships a clean
//! replacement we keep `unescape_value` and silence the lint at module
//! scope.

#![allow(
    deprecated,
    reason = "quick-xml 0.40 unescape_value deprecation; revisit when 0.41 lands with a replacement that decodes entities"
)]

use std::collections::HashMap;

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

use crate::ClientError;

/// One parsed `<item>` from a Newznab feed.
#[derive(Debug, Clone, Default)]
pub struct RawItem {
    /// `<title>...</title>` — release name.
    pub title: String,
    /// `<guid>...</guid>` — opaque indexer-side id, often a URL.
    pub guid: String,
    /// `<link>...</link>` or `<enclosure url=...>` — download URL for the
    /// `.nzb` (Newznab) or `.torrent` (Torznab).
    pub download_url: Option<String>,
    /// `<size>` or `length` attribute on `<enclosure>` — bytes.
    pub size_bytes: u64,
    /// `<category>` element (some indexers emit one), used as a fallback
    /// when no `category` attr is present.
    pub category: Option<String>,
    /// `<comments>` page URL.
    pub details_url: Option<String>,
    /// All `<newznab:attr name="X" value="Y"/>` entries, grouped by name.
    /// Some attrs can repeat (multiple `audio`, multiple `subs`); a Vec
    /// preserves that without forcing the caller to split strings.
    pub attrs: HashMap<String, Vec<String>>,
}

impl RawItem {
    /// First value for `name`, if present.
    #[must_use]
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .get(name)
            .and_then(|v| v.first())
            .map(String::as_str)
    }

    /// All values for `name`.
    #[must_use]
    pub fn attrs(&self, name: &str) -> &[String] {
        self.attrs.get(name).map_or(&[] as &[String], Vec::as_slice)
    }

    /// Convenience: `attr` parsed as `u32`.
    #[must_use]
    pub fn attr_u32(&self, name: &str) -> Option<u32> {
        self.attr(name).and_then(|s| s.parse().ok())
    }
}

/// Parsed top-level feed.
#[derive(Debug, Default)]
pub struct RawFeed {
    /// All `<item>` elements found in `<channel>`. Order preserved.
    pub items: Vec<RawItem>,
}

/// Parse an XML feed body into a [`RawFeed`].
///
/// # Errors
///
/// Returns [`ClientError::Xml`] on any quick-xml error or unexpected
/// structure (e.g. truncated body, unclosed element).
pub fn parse_feed(body: &str) -> Result<RawFeed, ClientError> {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);

    let mut feed = RawFeed::default();
    let mut current: Option<RawItem> = None;
    // Tracks the topmost element we're currently inside so we can route
    // text-bearing events to the right field (`title`, `guid`,
    // `comments`, ...).
    let mut text_target: Option<TextTarget> = None;
    // Accumulator across `Event::Text` and `Event::GeneralRef` between
    // the matching `<element>`/`</element>` pair. quick-xml 0.40 splits
    // text containing entities into multiple events (e.g. `<link>` with
    // `&amp;` arrives as Text("...?t=get"), GeneralRef("amp"),
    // Text("id=..."), ...). Replacing the field on every Text event,
    // as a naive parser would, drops everything past the first
    // separator and produces truncated URLs.
    let mut text_buf = String::new();

    loop {
        match reader.read_event() {
            Err(e) => {
                return Err(ClientError::Xml(format!(
                    "at {}: {e}",
                    reader.buffer_position()
                )));
            }
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                handle_start(&e, &mut current, &mut text_target);
                text_buf.clear();
            }
            Ok(Event::Empty(e)) => {
                handle_empty(&e, &mut current)?;
            }
            Ok(Event::End(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                // Flush any buffered text to the target field before
                // resetting state — guarantees we capture content
                // that arrived as multiple Text/GeneralRef events.
                if let (Some(item), Some(target)) = (current.as_mut(), text_target) {
                    if !text_buf.is_empty() {
                        apply_text(item, target, std::mem::take(&mut text_buf));
                    }
                }
                if name == "item" {
                    if let Some(it) = current.take() {
                        feed.items.push(it);
                    }
                }
                text_target = None;
                text_buf.clear();
            }
            Ok(Event::Text(t)) if text_target.is_some() => {
                let decoded = t
                    .decode()
                    .map_err(|err| ClientError::Xml(format!("decode: {err}")))?;
                text_buf.push_str(&decoded);
            }
            // Resolve `&amp;` / `&lt;` / `&gt;` / `&quot;` / `&apos;`
            // (the XML-1.0 predefined entity set). Custom entities
            // declared via `<!ENTITY>` aren't honored — Newznab feeds
            // don't declare any.
            Ok(Event::GeneralRef(r)) if text_target.is_some() => {
                let name = r
                    .decode()
                    .map_err(|err| ClientError::Xml(format!("entity decode: {err}")))?;
                if let Some(resolved) = quick_xml::escape::resolve_predefined_entity(&name) {
                    text_buf.push_str(resolved);
                }
            }
            Ok(Event::CData(c)) if text_target.is_some() => {
                let bytes = c.into_inner();
                let text = String::from_utf8(bytes.into_owned())
                    .map_err(|err| ClientError::Xml(format!("cdata utf-8: {err}")))?;
                text_buf.push_str(&text);
            }
            _ => {}
        }
    }
    Ok(feed)
}

#[derive(Debug, Clone, Copy)]
enum TextTarget {
    Title,
    Guid,
    Link,
    Comments,
    Category,
    Size,
}

fn handle_start(
    e: &BytesStart<'_>,
    current: &mut Option<RawItem>,
    text_target: &mut Option<TextTarget>,
) {
    let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
    match name.as_str() {
        "item" => {
            *current = Some(RawItem::default());
            *text_target = None;
        }
        "title" => *text_target = Some(TextTarget::Title),
        "guid" => *text_target = Some(TextTarget::Guid),
        "link" => *text_target = Some(TextTarget::Link),
        "comments" => *text_target = Some(TextTarget::Comments),
        "category" => *text_target = Some(TextTarget::Category),
        "size" => *text_target = Some(TextTarget::Size),
        _ => *text_target = None,
    }
}

fn handle_empty(e: &BytesStart<'_>, current: &mut Option<RawItem>) -> Result<(), ClientError> {
    let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
    if name.ends_with(":attr") || name == "attr" {
        // Newznab attr: name + value.
        let Some(item) = current.as_mut() else {
            return Ok(());
        };
        let mut attr_name = None;
        let mut attr_value = None;
        for attr in e.attributes().with_checks(false) {
            let attr = attr.map_err(|err| ClientError::Xml(format!("attr: {err}")))?;
            let key = String::from_utf8_lossy(attr.key.as_ref()).into_owned();
            let val = attr
                .unescape_value()
                .map_err(|err| ClientError::Xml(format!("unescape: {err}")))?
                .into_owned();
            match key.as_str() {
                "name" => attr_name = Some(val),
                "value" => attr_value = Some(val),
                _ => {}
            }
        }
        if let (Some(n), Some(v)) = (attr_name, attr_value) {
            item.attrs.entry(n).or_default().push(v);
        }
    } else if name == "enclosure" {
        // <enclosure url="..." length="..." type="..."/>
        let Some(item) = current.as_mut() else {
            return Ok(());
        };
        for attr in e.attributes().with_checks(false) {
            let attr = attr.map_err(|err| ClientError::Xml(format!("enclosure: {err}")))?;
            let key = String::from_utf8_lossy(attr.key.as_ref()).into_owned();
            let val = attr
                .unescape_value()
                .map_err(|err| ClientError::Xml(format!("unescape: {err}")))?
                .into_owned();
            match key.as_str() {
                "url" if item.download_url.is_none() => item.download_url = Some(val),
                "length" => {
                    if let Ok(n) = val.parse::<u64>() {
                        if item.size_bytes == 0 {
                            item.size_bytes = n;
                        }
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn apply_text(item: &mut RawItem, target: TextTarget, text: String) {
    match target {
        TextTarget::Title => item.title = text,
        TextTarget::Guid => item.guid = text,
        TextTarget::Link => {
            // `<link>` only counts as the download URL if `<enclosure>`
            // didn't set one already (Newznab tends to send both).
            if item.download_url.is_none() {
                item.download_url = Some(text);
            }
        }
        TextTarget::Comments => item.details_url = Some(text),
        TextTarget::Category => item.category = Some(text),
        TextTarget::Size => {
            if let Ok(n) = text.parse::<u64>() {
                if item.size_bytes == 0 {
                    item.size_bytes = n;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const SAMPLE_FEED: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:newznab="http://www.newznab.com/DTD/2010/feeds/attributes/">
  <channel>
    <title>NZBgeek</title>
    <newznab:response offset="0" total="2"/>
    <item>
      <title>The.Matrix.1999.BluRay.1080p.x264-FOO</title>
      <guid isPermaLink="true">https://api.example/details/abc123</guid>
      <link>https://api.example/api?t=get&amp;id=abc123&amp;apikey=KEY</link>
      <comments>https://api.example/details/abc123/comments</comments>
      <category>2040</category>
      <enclosure url="https://api.example/api?t=get&amp;id=abc123" length="9608016733" type="application/x-nzb"/>
      <newznab:attr name="category" value="2000"/>
      <newznab:attr name="category" value="2040"/>
      <newznab:attr name="size" value="9608016733"/>
      <newznab:attr name="grabs" value="123"/>
      <newznab:attr name="imdb" value="0133093"/>
      <newznab:attr name="tmdb" value="603"/>
      <newznab:attr name="language" value="English"/>
      <newznab:attr name="audio" value="English"/>
      <newznab:attr name="audio" value="Portuguese"/>
      <newznab:attr name="subs" value="English,Portuguese"/>
    </item>
    <item>
      <title>Other.Release</title>
      <guid>def456</guid>
      <enclosure url="https://api.example/api?t=get&amp;id=def456" length="100" type="application/x-nzb"/>
      <newznab:attr name="category" value="2040"/>
    </item>
  </channel>
</rss>"#;

    #[test]
    fn parses_two_items_with_attrs() {
        let feed = parse_feed(SAMPLE_FEED).unwrap();
        assert_eq!(feed.items.len(), 2);
        let it = &feed.items[0];
        assert_eq!(it.title, "The.Matrix.1999.BluRay.1080p.x264-FOO");
        assert_eq!(it.size_bytes, 9_608_016_733);
        assert_eq!(it.attr("imdb"), Some("0133093"));
        assert_eq!(it.attr("tmdb"), Some("603"));
        assert_eq!(it.attr_u32("tmdb"), Some(603));
        assert_eq!(
            it.attrs("audio"),
            &["English".to_string(), "Portuguese".to_string()]
        );
        assert_eq!(
            it.attrs("category"),
            &["2000".to_string(), "2040".to_string()]
        );
        assert_eq!(it.attr("subs"), Some("English,Portuguese"));
        assert!(it.download_url.as_deref().is_some());
    }

    #[test]
    fn second_item_has_minimum_set() {
        let feed = parse_feed(SAMPLE_FEED).unwrap();
        let it = &feed.items[1];
        assert_eq!(it.title, "Other.Release");
        assert_eq!(it.size_bytes, 100);
        assert!(it.attr("imdb").is_none());
    }

    #[test]
    fn link_text_unescapes_xml_entities() {
        // Regression: NZBGeek encodes ampersands in `<link>` as `&amp;`.
        // Without entity decoding the URL stayed `?t=get&amp;id=...`
        // and `Url::parse` interpreted the query string as a single
        // garbage pair, truncating the download URL in the probe view.
        let body = r#"<?xml version="1.0"?>
<rss xmlns:newznab="http://www.newznab.com/DTD/2010/feeds/attributes/">
  <channel>
    <item>
      <title>x</title>
      <guid>g</guid>
      <link>https://api.example/api?t=get&amp;id=abc123&amp;apikey=KEY</link>
    </item>
  </channel>
</rss>"#;
        let feed = parse_feed(body).unwrap();
        let url = feed.items[0].download_url.as_deref().unwrap();
        assert!(url.contains("id=abc123"), "entity not decoded; url = {url}");
        assert!(
            url.contains("apikey=KEY"),
            "entity not decoded past first ampersand; url = {url}"
        );
        // Inverse: the raw `&amp;` must NOT survive.
        assert!(!url.contains("&amp;"), "still entity-encoded; url = {url}");
    }

    #[test]
    fn empty_feed_yields_zero_items() {
        let body = r#"<?xml version="1.0"?><rss><channel></channel></rss>"#;
        let feed = parse_feed(body).unwrap();
        assert!(feed.items.is_empty());
    }

    #[test]
    fn malformed_xml_returns_error() {
        let body = "<rss><channel><item><title>unclosed</channel></rss>";
        let err = parse_feed(body).unwrap_err();
        assert!(matches!(err, ClientError::Xml(_)));
    }
}
