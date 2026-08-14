//! What a title *is*, as any provider can say it.
//!
//! The descriptive facet — title, synopsis, artwork, status — has a
//! single owner per item, unlike identity, which is a set. The reason is
//! that two providers describing one title do not disagree the way two
//! catalogues *number* one series: `title`, `year`, `original_title` and
//! `overview` all describe one cut of one work, and taking the title
//! from one and the year from another produces a record that describes
//! nothing. Precedence per field does not work, which is why this is one
//! struct with one source rather than fifteen fields with fifteen.
//!
//! It is safe to let policy override a recorded descriptive owner, and
//! **not** safe to do that for structure: rewriting a synopsis is cheap
//! and reversible, while rebuilding a tree re-points every acquisition
//! hanging off the item.

use time::OffsetDateTime;

use super::ids::MetadataSource;

/// Where a work stands, in brarr's own words.
///
/// The vocabulary is brarr's because it is compared, and a provider's is
/// not: TMDB says `Returning Series` for what `TheTVDB` calls
/// `Continuing`, and a column that accepts both is a column whose
/// comparisons depend on who wrote the row. Each provider maps its own
/// dialect into this at its own boundary.
///
/// Closed, and deliberately so — the same reason [`MetadataSource`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProductionStatus {
    /// Still airing, or between seasons.
    Returning,
    /// Finished as planned.
    Ended,
    /// Stopped before it was finished. Kept apart from [`Self::Ended`]
    /// because "will there be more?" has different answers.
    Cancelled,
    /// Announced and being made.
    InProduction,
    /// Out.
    Released,
    /// Announced, nothing shot.
    Announced,
}

impl ProductionStatus {
    /// Iteration order, defined by an exhaustive `match` rather than by
    /// an array literal — the same discipline `MetadataSource::next`
    /// follows, and for the same reason: an array never fails to compile
    /// for being short.
    const fn next(self) -> Option<Self> {
        match self {
            Self::Returning => Some(Self::Ended),
            Self::Ended => Some(Self::Cancelled),
            Self::Cancelled => Some(Self::InProduction),
            Self::InProduction => Some(Self::Released),
            Self::Released => Some(Self::Announced),
            Self::Announced => None,
        }
    }

    /// Every status, in a fixed order.
    pub fn all() -> impl Iterator<Item = Self> {
        std::iter::successors(Some(Self::Returning), |s| s.next())
    }

    /// The stored value.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Returning => "returning",
            Self::Ended => "ended",
            Self::Cancelled => "cancelled",
            Self::InProduction => "in-production",
            Self::Released => "released",
            Self::Announced => "announced",
        }
    }

    /// Inverse of [`Self::label`]. `None` for anything the storage
    /// `CHECK` would have refused, which only a hand-edited row can
    /// produce.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        Self::all().find(|s| s.label() == raw)
    }
}

/// A poster or backdrop, and how to read the value.
///
/// **The provenance is not precedence, it is grammar.** TMDB stores a
/// path relative to its image CDN (`/abc.jpg`) and `TheTVDB` returns an
/// absolute URL, so the same string means two different things and
/// cannot become a URL without knowing who wrote it. Prefixing the TMDB
/// CDN onto a `TheTVDB` URL 404s in silence, and the CSS guard checks
/// class names, never a URL value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artwork {
    /// Who issued it, which decides how [`Self::value`] is read.
    pub source: MetadataSource,
    /// A CDN-relative path or an absolute URL, per the source.
    pub value: String,
}

/// One title as one provider describes it.
///
/// Not `#[non_exhaustive]`: provider crates outside `brarr-core`
/// construct it, and the crate's rule applies to what the core hands
/// out, not to what it takes in — the same reasoning as
/// [`SeriesTree`](super::tree::SeriesTree).
#[derive(Debug, Clone)]
pub struct Description {
    /// Who described it. Written to `library_items.descriptive_source`,
    /// which is what a later refresh reads before asking anyone.
    pub source: MetadataSource,
    /// Localised title, in the language the provider was asked for.
    pub title: String,
    /// Original-language title, when the provider distinguishes one.
    pub original_title: Option<String>,
    /// Synopsis. May legitimately be empty — a provider with no
    /// translation returns nothing rather than the original, and saying
    /// nothing is better than saying it in a language nobody asked for.
    pub overview: Option<String>,
    /// Release or first-air year.
    pub year: Option<i32>,
    /// Where the work stands.
    pub status: Option<ProductionStatus>,
    /// Runtime in minutes; the episode average for a series.
    pub runtime_minutes: Option<i32>,
    /// Poster, when the provider has one.
    pub poster: Option<Artwork>,
    /// Backdrop, when the provider has one.
    pub backdrop: Option<Artwork>,
    /// Air date of the next unaired episode, when known.
    pub next_air_date: Option<OffsetDateTime>,
    /// Digital release date — searching before it is wasted effort.
    pub digital_release_at: Option<OffsetDateTime>,
    /// Physical release date.
    pub physical_release_at: Option<OffsetDateTime>,
}

impl Description {
    /// A description carrying nothing but a title and its source.
    ///
    /// Providers build on top of this with struct-update syntax, so a
    /// field a provider does not have is absent rather than invented,
    /// and adding a field does not touch every impl.
    #[must_use]
    pub fn new(source: MetadataSource, title: impl Into<String>) -> Self {
        Self {
            source,
            title: title.into(),
            original_title: None,
            overview: None,
            year: None,
            status: None,
            runtime_minutes: None,
            poster: None,
            backdrop: None,
            next_air_date: None,
            digital_release_at: None,
            physical_release_at: None,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    /// Every status round-trips through its stored form, and `all()`
    /// walks the enum rather than a hand-written list — so a variant
    /// added without a label fails to compile instead of shipping.
    #[test]
    fn every_status_round_trips_through_its_label() {
        let seen: Vec<_> = ProductionStatus::all().collect();
        assert_eq!(seen.len(), 6, "the chain covers the enum");
        for status in seen {
            assert_eq!(ProductionStatus::parse(status.label()), Some(status));
        }
        assert_eq!(ProductionStatus::parse("Returning Series"), None);
    }

    /// A provider fills what it has and nothing else — the absent field
    /// is absent, never a default that reads as an answer.
    #[test]
    fn a_new_description_invents_nothing() {
        let d = Description::new(MetadataSource::Tvdb, "Frieren");
        assert_eq!(d.source, MetadataSource::Tvdb);
        assert_eq!(d.title, "Frieren");
        assert!(d.overview.is_none());
        assert!(d.poster.is_none());
        assert!(d.status.is_none());
    }
}
