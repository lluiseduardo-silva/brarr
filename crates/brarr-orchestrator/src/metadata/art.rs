//! Building an image URL without naming a provider.
//!
//! The four call sites in `web/routes.rs` reached straight into
//! `brarr_tmdb::image_url`, which is a provider crate's type crossing
//! into the web layer — one of three such crossings, and the only one
//! that also hard-codes a CDN. It works for exactly as long as TMDB is
//! the only source of artwork.
//!
//! The two providers do not even store the same kind of value: TMDB
//! keeps a path *relative* to its image CDN (`/abc.jpg`) and TheTVDB
//! returns an absolute URL. That is why `library_items` records
//! `poster_source` beside `poster_path` — not to arbitrate between them,
//! but because the stored string cannot be read without knowing who
//! wrote it.
//!
//! ## Sizes are brarr's vocabulary, not TMDB's
//!
//! The call sites asked for `"w185"` and `"w342"`, which is TMDB's
//! spelling and means nothing to another provider. [`ImageSize`] names
//! the *surface* instead, and each provider maps it however it can —
//! TheTVDB has no size variants at all, so it maps every one of them to
//! the same URL rather than pretending.

use brarr_core::MetadataSource;

/// Which surface an image is for.
///
/// Naming the surface rather than the pixel width is what keeps the
/// mapping a provider's business. It also preserves the distinction the
/// call sites already made and would have been easy to lose: the index
/// renders 360 cards and the hero renders one, so they must not ask for
/// the same bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageSize {
    /// The library index and any other grid of many cards.
    Index,
    /// The detail screen's hero.
    Hero,
    /// A full-width backdrop.
    Backdrop,
}

/// Build an absolute image URL for a stored path.
///
/// `None` for an absent or blank path, so templates can branch on it —
/// the contract the previous helper had, kept.
#[must_use]
pub fn url(source: MetadataSource, path: Option<&str>, size: ImageSize) -> Option<String> {
    let path = path.map(str::trim).filter(|p| !p.is_empty())?;
    match source {
        MetadataSource::Tmdb => brarr_tmdb::image_url(Some(path), tmdb_size(size)),
        // TheTVDB stores artwork as an absolute URL, so there is nothing
        // to build and no size to pick. Anything that is not already a
        // URL is refused rather than pasted onto a CDN that would not
        // serve it.
        MetadataSource::Tvdb => path.starts_with("https://").then(|| path.to_owned()),
        // IMDb issues ids, not artwork. A row claiming otherwise renders
        // nothing rather than a broken image.
        MetadataSource::Imdb => None,
    }
}

/// TMDB's own spelling for a surface.
const fn tmdb_size(size: ImageSize) -> &'static str {
    match size {
        ImageSize::Index => "w185",
        ImageSize::Hero => "w342",
        ImageSize::Backdrop => "w1280",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The guard `css_coverage` cannot give.** That test checks class
    /// names and custom-property names and never the *value* of a
    /// declaration, which is how `bg-emerald-100` and
    /// `var(--color-on-accent-solid)` both shipped inert with the suite
    /// green. A CDN URL is the same shape of failure: a wrong one renders
    /// a broken image and breaks nothing.
    ///
    /// Walks the real enum, so a provider added without an artwork rule
    /// fails here rather than on screen.
    #[test]
    fn every_source_either_builds_an_absolute_url_or_declines() {
        for source in MetadataSource::all() {
            // A value shaped the way that source stores one.
            let stored = match source {
                MetadataSource::Tmdb => "/abc.jpg",
                MetadataSource::Tvdb => "https://artworks.thetvdb.com/banners/posters/1.jpg",
                MetadataSource::Imdb => "",
            };
            for size in [ImageSize::Index, ImageSize::Hero, ImageSize::Backdrop] {
                let built = url(source, Some(stored), size);
                if let Some(built) = built {
                    assert!(
                        built.starts_with("https://"),
                        "{source} built a relative URL: {built}"
                    );
                } else {
                    assert_eq!(
                        source,
                        MetadataSource::Imdb,
                        "{source} declined to build a URL for a value it stores"
                    );
                }
            }
        }
    }

    /// The index and the hero must not ask for the same bytes: one
    /// renders 360 cards, the other renders one.
    #[test]
    fn the_index_and_the_hero_ask_for_different_sizes() {
        let index = url(MetadataSource::Tmdb, Some("/abc.jpg"), ImageSize::Index);
        let hero = url(MetadataSource::Tmdb, Some("/abc.jpg"), ImageSize::Hero);
        assert_ne!(index, hero);
        assert_eq!(
            index.as_deref(),
            Some("https://image.tmdb.org/t/p/w185/abc.jpg")
        );
        assert_eq!(
            hero.as_deref(),
            Some("https://image.tmdb.org/t/p/w342/abc.jpg")
        );
    }

    #[test]
    fn an_absent_or_blank_path_builds_nothing() {
        for source in MetadataSource::all() {
            assert_eq!(url(source, None, ImageSize::Index), None);
            assert_eq!(url(source, Some("   "), ImageSize::Index), None);
        }
    }

    /// A TheTVDB row holding something that is not a URL renders nothing
    /// rather than being pasted onto TMDB's CDN, where it would 404.
    #[test]
    fn a_value_that_is_not_a_url_is_refused_rather_than_guessed() {
        assert_eq!(
            url(MetadataSource::Tvdb, Some("/abc.jpg"), ImageSize::Hero),
            None
        );
    }
}
