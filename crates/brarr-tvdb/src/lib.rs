//! Async client for `TheTVDB` v4 API — **a numbering source, not a
//! catalogue**.
//!
//! brarr's catalogue is TMDB's and stays that way. `library_items` is
//! keyed on the TMDB id, `library_episodes` holds TMDB's numbering, and
//! those two columns are simultaneously the row's identity, the file
//! name on disk and the pairing key with Sonarr — the reasoning
//! `migrations/20260808130000_episode_numbering.sql` sets out at length.
//! Making `TheTVDB` the source of truth for series would renumber all of
//! that, which is the exact damage the v0.14.0 release repaired.
//!
//! What `TheTVDB` is for is the one thing TMDB genuinely does not have:
//! **the split the scene actually uses**. Dragon Ball Super is one
//! season of 131 on TMDB and five of 14/13/19/30/55 here; Solo Leveling
//! is one of 25 there and two of 12 and 13 here. Releases follow this
//! one. So the data lands in `library_episode_numbering` — the same
//! translation table an operator-picked TMDB episode group writes to —
//! and nothing else moves.
//!
//! It also removes a circular dependency. brarr derives that numbering
//! from Sonarr today, which works only for as long as the \*arr brarr
//! means to replace is still installed to be read.
//!
//! # Season types
//!
//! [`SeasonType`] is the whole point: the same episodes come back under
//! different coordinates. `Official` is the broadcast split, `Absolute`
//! is one run of numbers straight through, and each episode carries its
//! own stable [`Episode::id`] — so two calls join into a translation
//! table rather than being guessed at.
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
mod model;

pub use client::{DEFAULT_BASE_URL, TvdbAuth, TvdbClient};
pub use error::TvdbError;
pub use model::{Episode, SeasonType, SeriesEpisodes};

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
