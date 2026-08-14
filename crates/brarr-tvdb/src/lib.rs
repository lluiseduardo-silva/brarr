//! Async client for `TheTVDB` v4 API — **the shape of a series, not the
//! catalogue**.
//!
//! brarr's catalogue is TMDB's: title, synopsis, artwork and the ids the
//! library is indexed by all come from there. What `TheTVDB` supplies is
//! the one thing TMDB genuinely does not have — **the split the scene
//! actually uses**. Dragon Ball Super is one season of 131 on TMDB and
//! five of 14/13/19/30/55 here; Solo Leveling is one of 25 there and two
//! of 12 and 13 here. Every release names the second.
//!
//! That difference used to be recorded beside a TMDB-built tree, in a
//! translation table read at eight points in two opposite directions.
//! It is not any more: a series is **born** with the tree its declared
//! structure owner builds, so the coordinate brarr stores is at once the
//! row identity, the query an indexer is sent, the marker matched in a
//! release name and the name written to disk. One value instead of two,
//! and nothing to keep in step.
//!
//! It also removes a circular dependency. brarr used to derive that
//! numbering from Sonarr, which works only for as long as the \*arr
//! brarr means to replace is still installed to be read.
//!
//! # Season types
//!
//! [`SeasonType`] is the whole point: the same episodes come back under
//! different coordinates. `Official` is the broadcast split and what
//! [`MetadataProvider::tree`](brarr_core::MetadataProvider::tree) builds
//! for `Ordering::Default`; `Absolute` is one run of numbers straight
//! through. Each episode carries its own stable [`Episode::id`] under
//! **every** season type, so two orderings of one series join by
//! identity and no heuristic is reached.
//!
//! # Auth
//!
//! `POST /login` with the project's `apikey` returns a bearer token good
//! for **one month**, cached here. A `pin` is sent only for a
//! user-supported key; brarr's is funded by the revenue tier, where
//! projects under $50k/year are free.
//!
//! # Attribution is a condition of that tier
//!
//! [`ATTRIBUTION`] and [`ATTRIBUTION_URL`] carry what the terms require,
//! verbatim, for the same reason `brarr-tmdb` does: the licence is
//! contingent on displaying it, so it is not a nicety the UI may drop.

#![forbid(unsafe_code)]

mod client;
mod dto;
mod error;
mod metadata_impl;
mod model;
mod retry;

pub use client::{DEFAULT_BASE_URL, DEFAULT_TIMEOUT, TvdbAuth, TvdbClient};
pub use error::TvdbError;
pub use model::{Episode, SeasonType, SeriesEpisodes};
pub use retry::{RetryConfig, is_transient};

/// What `TheTVDB`'s free tier requires be shown, verbatim.
///
/// > Unless approved by `TheTVDB`, attribution with a direct link to
/// > `TheTVDB.com` must be displayed to end users viewing metadata from
/// > our API.
///
/// brarr has a web UI, so a README line does not discharge it — the
/// allowance for "about or readme pages" is for command line products
/// and libraries.
pub const ATTRIBUTION: &str = "Metadados de séries fornecidos por TheTVDB.";

/// The direct link the attribution must carry.
pub const ATTRIBUTION_URL: &str = "https://www.thetvdb.com";
