//! Catalogue fixtures for tests.
//!
//! ## Why this exists before it is needed
//!
//! `NewLibraryItem.tmdb_id` is a required `i64` and it is written out in
//! full at sixty-odd places across fourteen files, plus every
//! `NewEpisode.tmdb_episode_id: None`. When identity becomes a set rather
//! than three named columns, every one of those is a **compilation**
//! failure of the whole crate, not a failing assertion — and a suite that
//! does not compile stops being an instrument for the rest of the work.
//!
//! So the fixtures go through one door first. The door is shaped so that
//! the change does not reach the call sites at all: a caller says
//! `Seed::series(76_479, "The Boys")` and this module decides what a TMDB
//! id *is*. Afterwards it builds an `ExternalId`; today it writes a
//! column. Neither is the caller's business.
//!
//! ## The duplicate, and why it is one
//!
//! `tests/support/mod.rs` carries the same API for the integration tests.
//! Integration tests link the crate compiled **without** `cfg(test)`, so a
//! `#[cfg(test)]` module here is invisible to them, and the alternatives
//! — a `test-support` feature with a self-dev-dependency, or `#[path]`
//! including this file into the test crate — both cost more than one
//! small file. Two places is still thirty times better than sixty.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test fixtures: a failure here is a broken test, not a runtime path"
)]

use super::library::{MediaType, NewEpisode, NewLibraryItem};

/// A catalogue entry under construction.
///
/// Fluent rather than a struct literal so that the identity fields are
/// reached through named methods. That is the whole point: the
/// constructors and [`Seed::imdb`] are what a later change re-implements,
/// and a caller writing `imdb_id: Some(...)` by hand would slip past it.
///
/// Carries only what the unit tests here actually set. The integration
/// side has its own, with the extras that side needs — speculative
/// helpers "for later" are what this workspace forbids, and an unused one
/// is dead code the compiler will say so about.
pub(crate) struct Seed {
    new: NewLibraryItem,
}

impl Seed {
    /// A film.
    pub(crate) fn movie(tmdb_id: i64, title: &str) -> Self {
        Self {
            new: NewLibraryItem {
                media_type: Some(MediaType::Movie),
                tmdb_id,
                title: title.to_owned(),
                ..NewLibraryItem::default()
            },
        }
    }

    /// A series.
    pub(crate) fn series(tmdb_id: i64, title: &str) -> Self {
        Self {
            new: NewLibraryItem {
                media_type: Some(MediaType::Tv),
                tmdb_id,
                title: title.to_owned(),
                ..NewLibraryItem::default()
            },
        }
    }

    /// Release year.
    pub(crate) fn year(mut self, year: i32) -> Self {
        self.new.year = Some(year);
        self
    }

    /// The IMDb id, in whichever convention the caller has.
    pub(crate) fn imdb(mut self, imdb: &str) -> Self {
        self.new.imdb_id = Some(imdb.to_owned());
        self
    }

    /// The TheTVDB id.
    pub(crate) fn tvdb(mut self, tvdb: i64) -> Self {
        self.new.tvdb_id = Some(tvdb);
        self
    }

    /// The value, unsaved.
    ///
    /// Every caller hands it straight to `library::upsert`, so the
    /// builder deliberately stops here rather than growing a `save` of
    /// its own: one door for identity, and the persistence stays where
    /// the test can see it.
    pub(crate) fn build(self) -> NewLibraryItem {
        self.new
    }
}

/// One episode, numbered and otherwise blank.
///
/// The identity field lives here and nowhere else, so the sites that set
/// a title or an air date do it with struct-update syntax on top and
/// never name it.
pub(crate) fn episode(number: i32) -> NewEpisode {
    NewEpisode {
        episode_number: number,
        title: None,
        air_date: None,
    }
}
