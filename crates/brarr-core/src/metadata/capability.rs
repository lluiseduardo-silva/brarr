//! What a provider answers for, asked before it is asked anything else.

use super::MediaType;

/// Which media kinds a capability covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaSupport {
    /// Not offered at all.
    None,
    /// Films only.
    Movies,
    /// Series only.
    Series,
    /// Both.
    Both,
}

impl MediaSupport {
    /// Whether this covers `media`.
    #[must_use]
    pub const fn covers(self, media: MediaType) -> bool {
        matches!(
            (self, media),
            (Self::Both, _) | (Self::Movies, MediaType::Movie) | (Self::Series, MediaType::Tv)
        )
    }
}

/// What a provider offers.
///
/// The registry consults this **before** dispatching, so a provider is
/// never asked something it does not do. That is what keeps
/// [`MetadataError::Unsupported`](super::MetadataError::Unsupported) a
/// bug report rather than a routine return value.
///
/// The failure being designed out is live in the sibling abstraction:
/// [`TrackerProvider`](crate::TrackerProvider) gives `search_by_tvdb` a
/// default of `Ok(vec![])`, so "I do not speak this axis" and "I found
/// nothing" are the same answer — every WASM plugin reports a healthy
/// zero for every series search and `/health` shows the provider green.
///
/// Not `#[non_exhaustive]`, unlike most public structs here: provider
/// crates outside this one construct it literally, and the rule exists
/// for what the core hands out, not for what it takes in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// Media kinds this provider can resolve external ids for.
    pub identity: MediaSupport,
    /// Media kinds this provider can build a season tree for.
    ///
    /// A film has no tree, so anything but [`MediaSupport::None`] or
    /// [`MediaSupport::Series`] is a declaration nothing can honour —
    /// [`Self::is_coherent`] refuses it and a guard walks every impl.
    pub structure: MediaSupport,
}

impl Capabilities {
    /// Whether the declaration is one a provider could actually meet.
    #[must_use]
    pub const fn is_coherent(self) -> bool {
        matches!(self.structure, MediaSupport::None | MediaSupport::Series)
    }
}

/// One credential field `/settings` should render for a provider.
///
/// Iterating these is what stops the settings form — already a
/// twenty-field struct posted whole — from growing a hand-written block
/// per provider, and what lets a guard assert that every declared
/// credential has an input and every input belongs to a source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CredentialField {
    /// Form field name, e.g. `tvdb_api_key`. Also the `settings` key.
    pub key: &'static str,
    /// Portuguese label — user-facing, like `ATTRIBUTION`.
    pub label: &'static str,
    /// Write-only in the form.
    ///
    /// A blank submission means "keep what is stored", never "erase":
    /// the field never echoes a stored credential back, so blank cannot
    /// carry the other meaning.
    pub secret: bool,
    /// Whether the provider is unusable without it.
    ///
    /// TheTVDB's PIN is the counterexample — it exists only for a
    /// user-supported key and must be **absent**, not empty, otherwise.
    pub required: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn support_covers_exactly_what_it_names() {
        assert!(MediaSupport::Both.covers(MediaType::Movie));
        assert!(MediaSupport::Both.covers(MediaType::Tv));
        assert!(MediaSupport::Movies.covers(MediaType::Movie));
        assert!(!MediaSupport::Movies.covers(MediaType::Tv));
        assert!(MediaSupport::Series.covers(MediaType::Tv));
        assert!(!MediaSupport::Series.covers(MediaType::Movie));
        assert!(!MediaSupport::None.covers(MediaType::Movie));
        assert!(!MediaSupport::None.covers(MediaType::Tv));
    }

    /// A film has no seasons, so a provider claiming to build one a tree
    /// is describing something that cannot exist.
    #[test]
    fn capabilities_never_claim_movie_structure() {
        assert!(
            !Capabilities {
                identity: MediaSupport::Both,
                structure: MediaSupport::Both,
            }
            .is_coherent()
        );
        assert!(
            !Capabilities {
                identity: MediaSupport::Both,
                structure: MediaSupport::Movies,
            }
            .is_coherent()
        );
        assert!(
            Capabilities {
                identity: MediaSupport::Both,
                structure: MediaSupport::Series,
            }
            .is_coherent()
        );
        assert!(
            Capabilities {
                identity: MediaSupport::Movies,
                structure: MediaSupport::None,
            }
            .is_coherent()
        );
    }
}
