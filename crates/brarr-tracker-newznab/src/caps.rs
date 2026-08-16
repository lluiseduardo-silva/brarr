//! What one indexer says it can be asked — the `t=caps` document,
//! reduced to the single question brarr puts to it.
//!
//! ## Why this exists
//!
//! **A Newznab server ignores a query parameter it does not know**, and
//! that is the whole problem. It does not answer `<error>`, it does not
//! return zero items: it drops the filter and answers the *unfiltered*
//! question. So `t=movie&tmdbid=1445721` against an indexer with no
//! TMDb support is not a search that finds nothing — it is a request
//! for the recent-movie feed, and every item in it looks like a
//! perfectly good result.
//!
//! Measured in production on 2026-08-15: a film brarr holds only a TMDb
//! id for pulled 25 arbitrary releases per sweep out of NZBGeek, and the
//! scanner grabbed the top-scoring one each time — sixteen unrelated
//! films, 317 GiB, all of them refused at import because they landed on
//! the same destination.
//!
//! `caps` is the server answering that question itself, so nothing is
//! guessed from the release names. NZBGeek advertises
//! `movie-search supportedParams="q,imdbid,genre"`; curupira.cc
//! advertises `"q,cat,limit,offset,imdbid,tmdbid"`. Both strings are
//! verbatim from those two servers and are the fixtures in
//! `tests/client_wiremock.rs`.
//!
//! ## Only the non-standard axis is checked
//!
//! `imdbid` on `movie-search` and `tvdbid`/`season`/`ep` on `tv-search`
//! are what the Newznab spec mandates, so they are taken on faith and
//! cost no round trip. `tmdbid` is a Torznab extension — it is the axis
//! that can be silently unsupported, and therefore the only one worth
//! the question.

use quick_xml::Reader;
use quick_xml::events::Event;

use crate::ClientError;

/// Query parameters one search function accepts.
///
/// An empty set means either "the server does not offer this function"
/// or "it offers it and named no parameters". The two are deliberately
/// the same value: both are answers brarr cannot filter with, and the
/// caller's decision is identical.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SupportedParams(Vec<String>);

impl SupportedParams {
    /// Whether the indexer named this parameter, case-insensitively.
    #[must_use]
    pub fn supports(&self, param: &str) -> bool {
        self.0.iter().any(|p| p.eq_ignore_ascii_case(param))
    }

    /// The parameters as advertised, for logging.
    #[must_use]
    pub fn as_slice(&self) -> &[String] {
        &self.0
    }

    /// Split a `supportedParams="q,imdbid,genre"` value.
    fn parse(raw: &str) -> Self {
        Self(
            raw.split(',')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(str::to_owned)
                .collect(),
        )
    }
}

/// The `<searching>` block of a `t=caps` document.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Capabilities {
    /// `<movie-search supportedParams="…"/>`.
    pub movie_search: SupportedParams,
    /// `<tv-search supportedParams="…"/>`.
    pub tv_search: SupportedParams,
}

/// Read a `t=caps` document.
///
/// **A document this cannot make sense of yields empty capabilities, not
/// an error**, and that is the safe direction: the only caller is the
/// non-standard axis, so "I could not tell" and "not supported" both
/// mean *do not send it*. A server that genuinely supports `tmdbid` and
/// publishes an unreadable caps document loses one axis; a server that
/// does not, and is asked anyway, returns a feed of strangers.
///
/// `available="no"` is honoured explicitly: a function the server says
/// it does not offer names no usable parameter regardless of what the
/// `supportedParams` attribute happens to hold.
///
/// # Errors
///
/// [`ClientError::Xml`] only when the document is not well-formed XML at
/// all — a truncated body or an HTML error page, which is worth telling
/// the operator about rather than reading as "no capabilities".
pub(crate) fn parse(xml: &str) -> Result<Capabilities, ClientError> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut caps = Capabilities::default();

    loop {
        let event = reader
            .read_event()
            .map_err(|err| ClientError::Xml(format!("caps: {err}")))?;
        match event {
            Event::Eof => break,
            Event::Start(e) | Event::Empty(e) => {
                let name = e.name();
                let local = name.local_name();
                let slot = match local.as_ref() {
                    b"movie-search" => &mut caps.movie_search,
                    b"tv-search" => &mut caps.tv_search,
                    _ => continue,
                };
                let mut available = true;
                let mut params = SupportedParams::default();
                for attr in e.attributes().flatten() {
                    let value = String::from_utf8_lossy(&attr.value).into_owned();
                    match attr.key.local_name().as_ref() {
                        b"available" => available = !value.trim().eq_ignore_ascii_case("no"),
                        b"supportedParams" => params = SupportedParams::parse(&value),
                        _ => {}
                    }
                }
                if available {
                    *slot = params;
                }
            }
            _ => {}
        }
    }
    Ok(caps)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert on happy paths"
)]
mod tests {
    use super::*;

    /// Verbatim from `https://api.nzbgeek.info/api?t=caps`, 2026-08-15.
    const NZBGEEK: &str = r#"<caps><searching>
        <search available="yes" supportedParams="q,group"/>
        <tv-search available="yes" supportedParams="q,rid,tvdbid,tvmazeid,season,ep"/>
        <movie-search available="yes" supportedParams="q,imdbid,genre"/>
        </searching></caps>"#;

    /// Verbatim from `https://curupira.cc/api?t=caps`, same day.
    const CURUPIRA: &str = r#"<caps><searching>
        <tv-search available="yes" supportedParams="q,season,ep,cat,limit,offset,rid,tvdbid,tmdbid,imdbid,malid" />
        <movie-search available="yes" supportedParams="q,cat,limit,offset,imdbid,tmdbid" />
        </searching></caps>"#;

    #[test]
    fn nzbgeek_does_not_offer_tmdbid_on_movie_search() {
        let caps = parse(NZBGEEK).unwrap();
        assert!(caps.movie_search.supports("imdbid"));
        assert!(!caps.movie_search.supports("tmdbid"));
        // The TV axis brarr actually uses is there, which is why the
        // episode path was never affected by this.
        assert!(caps.tv_search.supports("tvdbid"));
        assert!(caps.tv_search.supports("ep"));
    }

    #[test]
    fn curupira_does_offer_it() {
        let caps = parse(CURUPIRA).unwrap();
        assert!(caps.movie_search.supports("tmdbid"));
        assert!(caps.movie_search.supports("imdbid"));
    }

    #[test]
    fn the_match_ignores_case_and_padding() {
        let caps = parse(
            r#"<caps><searching><movie-search supportedParams=" q , TMDbID "/></searching></caps>"#,
        )
        .unwrap();
        assert!(caps.movie_search.supports("tmdbid"));
    }

    #[test]
    fn a_function_the_server_disclaims_supports_nothing() {
        // `available="no"` alongside a populated attribute is not a
        // contradiction to resolve — the server said no.
        let caps =
            parse(r#"<caps><searching><movie-search available="no" supportedParams="q,tmdbid"/></searching></caps>"#)
                .unwrap();
        assert!(!caps.movie_search.supports("tmdbid"));
    }

    #[test]
    fn a_caps_document_naming_no_params_supports_nothing() {
        // The shape that made this whole class of bug possible: the
        // server offers movie-search and says nothing about how it can
        // be filtered. Absence of evidence is not permission.
        let caps = parse(r#"<caps><searching><movie-search available="yes"/></searching></caps>"#)
            .unwrap();
        assert!(!caps.movie_search.supports("tmdbid"));
        assert!(!caps.movie_search.supports("imdbid"));
    }

    #[test]
    fn a_namespaced_document_is_read_the_same() {
        // Some forks emit the whole caps tree under a prefix. The local
        // name is what carries the meaning.
        let caps = parse(
            r#"<n:caps xmlns:n="urn:x"><n:searching><n:movie-search supportedParams="tmdbid"/></n:searching></n:caps>"#,
        )
        .unwrap();
        assert!(caps.movie_search.supports("tmdbid"));
    }

    #[test]
    fn an_html_error_page_is_an_error_not_an_empty_answer() {
        // A login page answered with 200 must not read as "this server
        // supports nothing" — that is indistinguishable from a real
        // answer, and the operator would never learn the key was wrong.
        assert!(parse("<html><body><p>Forbidden</body></html>").is_err());
    }
}
