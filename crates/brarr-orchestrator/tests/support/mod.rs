//! Catalogue fixtures for the integration tests.
//!
//! The twin of `src/db/seed.rs`, and it is a twin rather than a shared
//! module because integration tests link the crate compiled **without**
//! `cfg(test)`: a `#[cfg(test)]` module in the library is invisible from
//! here. The alternatives — a `test-support` feature with a
//! self-dev-dependency, or `#[path]`-including the library's file into
//! this crate — both cost more than one small file.
//!
//! Same reason for existing: `NewLibraryItem.tmdb_id` is a required
//! `i64`, and when identity becomes a set rather than three named
//! columns every literal is a compilation failure of the whole test
//! binary. Routing them through one door means the change lands here and
//! the call sites do not notice.

#![allow(dead_code, reason = "each integration test uses a subset")]

use brarr_orchestrator::db::Pool;
use brarr_orchestrator::db::library::{
    self, LibraryItem, MediaType, NewEpisode, NewLibraryItem, NewSeason,
};
use time::OffsetDateTime;
use uuid::Uuid;

/// A catalogue entry under construction.
pub struct Seed {
    new: NewLibraryItem,
}

impl Seed {
    /// A film.
    pub fn movie(tmdb_id: i64, title: &str) -> Self {
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
    pub fn series(tmdb_id: i64, title: &str) -> Self {
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
    pub fn year(mut self, year: i32) -> Self {
        self.new.year = Some(year);
        self
    }

    /// The IMDb id, in whichever convention the caller has.
    pub fn imdb(mut self, imdb: &str) -> Self {
        self.new.imdb_id = Some(imdb.to_owned());
        self
    }

    /// The TheTVDB id.
    pub fn tvdb(mut self, tvdb: i64) -> Self {
        self.new.tvdb_id = Some(tvdb);
        self
    }

    /// Anything else — artwork, dates, a status.
    ///
    /// Deliberately **not** an identity escape hatch: those go through
    /// the named methods above, so the places a change has to visit stay
    /// findable with a grep for this module.
    pub fn with(mut self, f: impl FnOnce(&mut NewLibraryItem)) -> Self {
        f(&mut self.new);
        self
    }

    /// The value, unsaved.
    pub fn build(self) -> NewLibraryItem {
        self.new
    }

    /// Persist and return the stored row.
    ///
    /// # Panics
    ///
    /// If the write fails. A broken fixture is a broken test.
    pub async fn save(self, pool: &Pool) -> LibraryItem {
        library::upsert(pool, &self.new)
            .await
            .expect("seed the catalogue entry")
    }
}

/// One episode, numbered and otherwise blank.
///
/// The identity field lives here and nowhere else; a site that wants a
/// title or an air date builds on top with struct-update syntax and never
/// names it.
pub fn episode(number: i32) -> NewEpisode {
    NewEpisode {
        tmdb_episode_id: None,
        episode_number: number,
        title: None,
        air_date: None,
    }
}

/// One episode with an air date — what separates "missing" from "not out
/// yet" on every coverage assertion.
pub fn episode_at(number: i32, aired: OffsetDateTime) -> NewEpisode {
    NewEpisode {
        air_date: Some(aired),
        ..episode(number)
    }
}

/// A season holding exactly these episodes.
pub fn season(number: i32, episodes: Vec<NewEpisode>) -> NewSeason {
    NewSeason {
        season_number: number,
        episode_count: i32::try_from(episodes.len()).unwrap_or(0),
        air_date: None,
        episodes,
    }
}

/// A season of `count` blank episodes, numbered from 1.
pub fn full_season(number: i32, count: i32) -> NewSeason {
    season(number, (1..=count).map(episode).collect())
}

/// Write a tree.
///
/// # Panics
///
/// If the write fails.
pub async fn tree(pool: &Pool, item: Uuid, seasons: &[NewSeason]) {
    library::sync_seasons(pool, item, seasons)
        .await
        .expect("seed the tree");
}
