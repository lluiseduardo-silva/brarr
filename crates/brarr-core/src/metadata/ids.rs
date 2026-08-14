//! Who a title is, according to whom.

use std::fmt;

/// Movie or series.
///
/// Declared here rather than in the orchestrator's data layer, where it
/// was declared exactly once: a provider trait that dispatches on media
/// kind cannot depend on `db::library` without collapsing the boundary
/// this workspace keeps. `db::library` re-exports it, so call sites do
/// not change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaType {
    /// A single film.
    Movie,
    /// A series, with seasons and episodes hanging off it.
    Tv,
}

impl MediaType {
    /// Short tag, as stored in `library_items.media_type`.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Movie => "movie",
            Self::Tv => "tv",
        }
    }

    /// Inverse of [`Self::label`]. `None` for anything the CHECK would
    /// have refused.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "movie" => Some(Self::Movie),
            "tv" => Some(Self::Tv),
            _ => None,
        }
    }
}

impl fmt::Display for MediaType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// What a source *is*, which decides what may be asked of it.
///
/// Mirrors the `metadata_sources.kind` column. The distinction is not
/// decoration: the registry builds a client for one kind and must never
/// try for the other, while both are legitimate keys in
/// `library_item_ids`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// A crate talks to it, and it answers questions.
    Provider,
    /// It issues ids brarr stores and passes to trackers, but brarr
    /// never queries it. IMDb is the whole category.
    Namespace,
}

/// Every catalogue brarr knows a title by.
///
/// Deliberately an enum and deliberately **not** `#[non_exhaustive]`:
/// exhaustive `match` is this workspace's dominant safety mechanism, and
/// the whole cost of adding a provider should be a list of compiler
/// errors. A guard scans the workspace for a `_ =>` arm over this type.
///
/// The database counterpart is a seeded table with a foreign key, never
/// a CHECK, so adding a source is an `INSERT` and an unregistered one is
/// a constraint violation on first write instead of a value that is
/// valid in Rust and inert in SQLite. That is not hypothetical: a
/// `'tvdb'` missing from one CHECK made every write of a TheTVDB-derived
/// numbering die in production with the suite green.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MetadataSource {
    /// The Movie Database.
    Tmdb,
    /// TheTVDB.
    Tvdb,
    /// IMDb — an id namespace brarr stores and never queries.
    Imdb,
}

// There is deliberately no `Manual`. The old numbering column had one and
// nothing in this schema can carry it: blocks the operator declares are
// `structure_family = 'manual'` with the provider still owning
// `structure_source`, and an id the operator types by hand is that
// namespace's id with `verified_at` left NULL. A variant nothing can
// write is one every `match` has to answer for anyway.

impl MetadataSource {
    /// Iteration order, defined by an exhaustive `match` rather than by
    /// an array literal.
    ///
    /// This is the difference between a guard that promises and a guard
    /// that catches. An array never fails to compile for being short, so
    /// a new variant ships uncovered with the suite green — which is
    /// exactly how `every_status_tone_has_a_rule` can pass over a tone it
    /// does not list. Adding a variant here breaks `next`, and the author
    /// has to say where it belongs.
    const fn next(self) -> Option<Self> {
        match self {
            Self::Tmdb => Some(Self::Tvdb),
            Self::Tvdb => Some(Self::Imdb),
            Self::Imdb => None,
        }
    }

    /// Every source, in a fixed order. What all the guards walk.
    pub fn all() -> impl Iterator<Item = Self> {
        std::iter::successors(Some(Self::Tmdb), |s| s.next())
    }

    /// The `metadata_sources.label` value.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Tmdb => "tmdb",
            Self::Tvdb => "tvdb",
            Self::Imdb => "imdb",
        }
    }

    /// What the screens call it.
    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Tmdb => "TMDB",
            Self::Tvdb => "TheTVDB",
            Self::Imdb => "IMDb",
        }
    }

    /// What may be asked of it. See [`SourceKind`].
    #[must_use]
    pub const fn kind(self) -> SourceKind {
        match self {
            Self::Tmdb | Self::Tvdb => SourceKind::Provider,
            Self::Imdb => SourceKind::Namespace,
        }
    }

    /// Inverse of [`Self::label`]. `None` for anything unregistered.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        Self::all().find(|s| s.label() == raw)
    }
}

impl fmt::Display for MetadataSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.display_name())
    }
}

/// A media identity as one source writes it.
///
/// `value` is private and canonicalised on construction, which is what
/// makes the two IMDb conventions in this codebase impossible to mix:
/// `library_items.imdb_id` holds `ttNNNNNNN` while `searches.imdb_id`
/// holds the bare number, reconciled today by one helper plus several
/// independent reimplementations that disagree about leading zeros.
///
/// Stored as text because the convention belongs to the source. An
/// integer column here would be a bet that no future provider uses a
/// slug, and this workspace has already paid for that bet once: a
/// `parse::<u64>().unwrap_or(0)` over non-numeric Newznab guids
/// collapsed every one of them onto key `0`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ExternalId {
    source: MetadataSource,
    value: Box<str>,
}

impl ExternalId {
    /// Build and canonicalise.
    ///
    /// IMDb accepts every convention this codebase has ever written —
    /// `133093`, `tt133093`, `tt0133093` — and stores one. TMDB and
    /// TheTVDB accept a positive integer.
    ///
    /// # Errors
    ///
    /// [`ExternalIdError::Empty`] for a blank value, and
    /// [`ExternalIdError::Malformed`] for anything not shaped the way the
    /// source shapes it.
    pub fn new(source: MetadataSource, raw: &str) -> Result<Self, ExternalIdError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(ExternalIdError::Empty { origin: source });
        }
        let value = match source {
            MetadataSource::Imdb => canonical_imdb(trimmed)?,
            MetadataSource::Tmdb | MetadataSource::Tvdb => canonical_numeric(source, trimmed)?,
        };
        Ok(Self {
            source,
            value: value.into_boxed_str(),
        })
    }

    /// Who issued it.
    #[must_use]
    pub const fn source(&self) -> MetadataSource {
        self.source
    }

    /// The canonical text, as it is stored.
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }

    /// The numeric form the tracker fan-out needs, `tt` stripped.
    ///
    /// # Errors
    ///
    /// [`ExternalIdError::Malformed`] — and **never** a silent `None`.
    /// An id brarr holds but cannot search with has to be reported: the
    /// alternative drops the title out of the sweep with no counter and
    /// no log, and the screen then blames the trackers.
    pub fn as_u32(&self) -> Result<u32, ExternalIdError> {
        let digits = self.value.strip_prefix("tt").unwrap_or(&self.value);
        digits
            .parse::<u32>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| ExternalIdError::Malformed {
                origin: self.source,
                value: self.value.to_string(),
            })
    }
}

impl fmt::Display for ExternalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.source.label(), self.value)
    }
}

/// `tt` plus at least seven digits, which is the form IMDb itself uses
/// and the one `library_items.imdb_id` stores. Ids past seven digits
/// keep their length — `tt10872600` is real.
fn canonical_imdb(raw: &str) -> Result<String, ExternalIdError> {
    let digits = raw.strip_prefix("tt").unwrap_or(raw);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ExternalIdError::Malformed {
            origin: MetadataSource::Imdb,
            value: raw.to_owned(),
        });
    }
    let trimmed = digits.trim_start_matches('0');
    if trimmed.is_empty() {
        return Err(ExternalIdError::Malformed {
            origin: MetadataSource::Imdb,
            value: raw.to_owned(),
        });
    }
    Ok(format!("tt{trimmed:0>7}"))
}

/// A positive integer with no leading zeros, so `0603` and `603` are one
/// key rather than two rows under the same UNIQUE index.
fn canonical_numeric(source: MetadataSource, raw: &str) -> Result<String, ExternalIdError> {
    raw.parse::<u64>()
        .ok()
        .filter(|n| *n > 0)
        .map(|n| n.to_string())
        .ok_or_else(|| ExternalIdError::Malformed {
            origin: source,
            value: raw.to_owned(),
        })
}

/// Why a value could not become an [`ExternalId`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExternalIdError {
    /// Nothing but whitespace.
    #[error("{origin} id cannot be empty")]
    Empty {
        /// The source that was asked.
        origin: MetadataSource,
    },
    /// Not the shape this source uses.
    #[error("`{value}` is not the shape a {origin} id has")]
    Malformed {
        /// The source that was asked.
        origin: MetadataSource,
        /// What was offered.
        value: String,
    },
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    /// **The guard that makes adding a provider a compile error.**
    ///
    /// `all()` is built from `next()`, an exhaustive `match`, so a new
    /// variant cannot reach this test without the author placing it in
    /// the chain. A hand-written array would simply be short, and short
    /// arrays compile.
    #[test]
    fn all_lists_every_variant() {
        let listed: Vec<_> = MetadataSource::all().collect();
        assert_eq!(listed.len(), 3, "a variant is missing from next()");
        for source in MetadataSource::all() {
            assert!(!source.label().is_empty());
            assert!(!source.display_name().is_empty());
            assert_eq!(MetadataSource::parse(source.label()), Some(source));
        }
        // No two share a label, or the FK would point at one row for two
        // meanings.
        let labels: std::collections::HashSet<_> =
            MetadataSource::all().map(MetadataSource::label).collect();
        assert_eq!(labels.len(), listed.len());
    }

    #[test]
    fn an_unregistered_label_does_not_parse() {
        assert_eq!(MetadataSource::parse("anilist"), None);
        assert_eq!(MetadataSource::parse(""), None);
    }

    /// **The two IMDb conventions become one value.**
    ///
    /// `library_items.imdb_id` stores `tt0133093` and `searches.imdb_id`
    /// stores `133093`, reconciled in several places that disagree about
    /// leading zeros. Whichever one arrives, one thing is stored.
    #[test]
    fn an_imdb_id_is_canonical_whichever_convention_it_arrives_in() {
        let canonical = ExternalId::new(MetadataSource::Imdb, "tt0133093").unwrap();
        for raw in ["133093", "tt133093", "tt0133093", " tt0133093 "] {
            assert_eq!(
                ExternalId::new(MetadataSource::Imdb, raw).unwrap(),
                canonical,
                "{raw} did not canonicalise"
            );
        }
        assert_eq!(canonical.value(), "tt0133093");
        assert_eq!(canonical.as_u32().unwrap(), 133_093);
    }

    /// An id longer than seven digits keeps its length rather than being
    /// truncated into somebody else's film.
    #[test]
    fn a_long_imdb_id_is_left_alone() {
        let id = ExternalId::new(MetadataSource::Imdb, "tt10872600").unwrap();
        assert_eq!(id.value(), "tt10872600");
        assert_eq!(id.as_u32().unwrap(), 10_872_600);
    }

    /// `0603` and `603` are the same title, and the UNIQUE index has to
    /// see them as one row.
    #[test]
    fn a_numeric_id_drops_its_leading_zeros() {
        let a = ExternalId::new(MetadataSource::Tmdb, "0603").unwrap();
        let b = ExternalId::new(MetadataSource::Tmdb, "603").unwrap();
        assert_eq!(a, b);
        assert_eq!(a.value(), "603");
    }

    /// Zero is how this codebase spells "no id". It must not become one.
    #[test]
    fn zero_is_not_an_id() {
        for source in [
            MetadataSource::Tmdb,
            MetadataSource::Tvdb,
            MetadataSource::Imdb,
        ] {
            assert!(ExternalId::new(source, "0").is_err(), "{source} accepted 0");
        }
        assert!(ExternalId::new(MetadataSource::Imdb, "tt0000000").is_err());
    }

    /// Every source is classified, so a new one cannot inherit a default.
    #[test]
    fn every_source_declares_what_it_is() {
        let providers: Vec<_> = MetadataSource::all()
            .filter(|s| s.kind() == SourceKind::Provider)
            .collect();
        assert_eq!(
            providers,
            vec![MetadataSource::Tmdb, MetadataSource::Tvdb],
            "the providers are the two with a client crate"
        );
        // Every source can key an item; the kind decides whether brarr
        // ever *asks* one anything, not whether it can be keyed on.
        for source in MetadataSource::all() {
            assert!(
                ExternalId::new(source, "1").is_ok(),
                "{source} cannot key an item"
            );
        }
    }

    #[test]
    fn a_malformed_value_is_refused_rather_than_coerced() {
        assert!(ExternalId::new(MetadataSource::Tmdb, "abc").is_err());
        assert!(ExternalId::new(MetadataSource::Imdb, "tt").is_err());
        assert!(ExternalId::new(MetadataSource::Tvdb, "  ").is_err());
        assert!(ExternalId::new(MetadataSource::Imdb, "tt12x4567").is_err());
    }

    #[test]
    fn media_type_round_trips_through_its_label() {
        for media in [MediaType::Movie, MediaType::Tv] {
            assert_eq!(MediaType::parse(media.label()), Some(media));
        }
        assert_eq!(MediaType::parse("anime"), None);
    }
}
