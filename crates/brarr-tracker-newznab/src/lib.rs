//! `brarr-tracker-newznab` — HTTP client for Newznab/Torznab indexers.
//!
//! Newznab is the de-facto API used by Usenet indexers (NZBGeek,
//! DrunkenSlug, NZB.su, etc.). Torznab is the same protocol with a
//! `torznab:` XML namespace tag and a `<link>` pointing at a `.torrent`
//! file instead of a `.nzb`. From brarr's perspective the response
//! shape is identical, so this crate handles both.
//!
//! ## Capabilities discovery
//!
//! Most indexers publish `/api?t=caps&apikey=KEY` which returns an
//! XML document listing supported search axes (movie-search by IMDb,
//! tv-search by TVDB, etc.). We don't poll caps at runtime — the
//! orchestrator knows the configured tracker `kind` and dispatches
//! searches it expects to work. If the indexer doesn't support the
//! axis, the response is an empty `<channel>` (treated as zero hits).
//!
//! ## Movie search
//!
//! NZBGeek `caps` confirms the standard:
//! `?t=movie&imdbid=<numeric IMDb tt-id without the leading "tt">&apikey=KEY`.
//! TMDb is **not** a supported parameter on NZBGeek's movie-search
//! endpoint, so this client only honors [`brarr_core::TrackerProvider::search_by_imdb`].
//! Callers without an IMDb id should rely on UNIT3D providers.
//!
//! ## Newznab-attr → `ReleaseEnrichment` mapping
//!
//! Newznab responses carry a flat list of `<newznab:attr name="..." value="..."/>`
//! elements per `<item>`. Relevant ones:
//! - `language` — primary audio language (free-form: `"English"`, `"Portuguese"`)
//! - `audio` — one or more audio tracks; sometimes comma-joined, sometimes repeated
//! - `subs` — same shape as `audio` but for subtitles
//! - `size`, `grabs`, `imdb`, `tmdb`, `tvdbid`
//!
//! The parser collects all repeats of the same `name` and the
//! converter then normalizes each language string through
//! [`brarr_core::Language`]'s parser.

#![allow(
    clippy::module_name_repetitions,
    clippy::doc_markdown,
    reason = "Newznab/Torznab/IMDb/TMDb/TVDB/UNIT3D appear in module docs frequently"
)]

mod client;
mod convert;
mod dto;
mod error;

pub use client::NewznabClient;
pub use error::ClientError;
