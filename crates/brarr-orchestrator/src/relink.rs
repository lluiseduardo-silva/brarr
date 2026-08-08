//! Re-point grabs that lost the episode they hold.
//!
//! [`crate::db::library::sync_seasons`] used to rebuild the season tree
//! with fresh UUIDs on every metadata refresh, and `grabs.episode_id` is
//! `ON DELETE SET NULL` — so every per-episode grab of every series was
//! unlinked, over and over, because the passive \*arr sweep calls that
//! function for each series on each pass. The source is fixed; the rows
//! it already produced are still in the database.
//!
//! They do not read as damage, which is why this pass exists rather than
//! a note in the changelog: `(episode_id NULL, season_number NULL)` is
//! the encoding of "covers the whole item", so a single file answers for
//! the entire show and the library renders **complete**. Nothing asks
//! the operator for action and the scanner stops looking.
//!
//! What this repairs is what it can **positively identify**, and nothing
//! else. The file name is the evidence: brarr's own importer wrote
//! `Título - S01E02.mkv` ([`crate::import::destination`]), so the marker
//! in the name is the episode the file is. A file with no marker — the
//! whole of an absolute-numbered anime catalogue — is left alone here
//! and healed on the \*arr path instead, in [`crate::arr_import`], where
//! Sonarr's own file-to-episode pairing is available. Guessing is not on
//! the table: a wrong link is worse than an orphan, because it looks
//! right.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use tracing::{debug, info};
use uuid::Uuid;

use crate::db::{Pool, grabs, library};
use crate::{AppError, adopt};

/// What one repair pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RelinkReport {
    /// Orphans considered.
    pub examined: usize,
    /// Orphans pointed back at their episode.
    pub linked: usize,
    /// File names carrying no unambiguous `SxxEyy`. Not a failure — the
    /// \*arr path is where these get answered.
    pub no_marker: usize,
    /// Markers naming an episode the catalogue does not have.
    pub unknown_episode: usize,
    /// Episodes already covered by a live grab of the same release.
    pub occupied: usize,
}

impl RelinkReport {
    /// Whether anything at all was found to look at.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.examined == 0
    }
}

/// Repair every orphan whose file name names its episode.
///
/// Idempotent by construction: [`grabs::relink_episode`] only ever fills
/// a blank, and an orphan it could not identify stays an orphan, so a
/// second pass over the same database examines the same rows and changes
/// nothing.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn run(pool: &Pool) -> Result<RelinkReport, AppError> {
    let orphans = grabs::unlinked_episode_grabs(pool).await?;
    let mut report = RelinkReport::default();

    // One tree per series, not one per grab: a repair on a long-running
    // show is hundreds of rows against the same catalogue.
    let items: HashSet<Uuid> = orphans.iter().map(|g| g.item_id).collect();
    let mut trees: HashMap<Uuid, HashMap<(i32, i32), Uuid>> = HashMap::with_capacity(items.len());
    for item_id in items {
        let tree = library::episodes(pool, item_id)
            .await?
            .into_iter()
            .map(|e| ((e.season_number, e.episode_number), e.id))
            .collect();
        trees.insert(item_id, tree);
    }

    for grab in &orphans {
        report.examined += 1;
        let Some(path) = grab.imported_path.as_deref() else {
            continue;
        };
        let Some(name) = Path::new(path).file_name() else {
            report.no_marker += 1;
            continue;
        };
        let Ok((season, number)) = adopt::parse_marker(&name.to_string_lossy()) else {
            report.no_marker += 1;
            continue;
        };

        let key = (i32::from(season), i32::from(number));
        let Some(&episode_id) = trees.get(&grab.item_id).and_then(|t| t.get(&key)) else {
            report.unknown_episode += 1;
            continue;
        };

        match grabs::relink_episode(pool, grab.id, episode_id).await? {
            grabs::Relink::Linked => {
                report.linked += 1;
                debug!(
                    target: "brarr_orchestrator::relink",
                    grab = %grab.id, path, season, number,
                    "re-pointed an orphaned grab at its episode"
                );
            }
            grabs::Relink::Occupied => report.occupied += 1,
            grabs::Relink::AlreadyLinked => {}
        }
    }

    if !report.is_empty() {
        info!(
            target: "brarr_orchestrator::relink",
            examined = report.examined,
            linked = report.linked,
            no_marker = report.no_marker,
            unknown_episode = report.unknown_episode,
            occupied = report.occupied,
            "repaired grabs unlinked by a metadata refresh"
        );
    }
    Ok(report)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert on happy paths"
)]
mod tests {
    use super::*;
    use crate::db::grabs::{GrabStatus, NewGrab, mark_imported, reserve, set_status};
    use crate::db::library::{
        MediaType, NewEpisode, NewLibraryItem, NewSeason, sync_seasons, upsert,
    };
    use crate::db::open_memory;

    async fn series(pool: &Pool) -> (Uuid, Uuid) {
        let item = upsert(
            pool,
            &NewLibraryItem {
                media_type: Some(MediaType::Tv),
                tmdb_id: 62715,
                title: "Dragon Ball Super".to_owned(),
                ..NewLibraryItem::default()
            },
        )
        .await
        .unwrap();
        sync_seasons(
            pool,
            item.id,
            &[NewSeason {
                season_number: 1,
                episode_count: 2,
                air_date: None,
                episodes: vec![
                    NewEpisode {
                        episode_number: 1,
                        title: None,
                        air_date: None,
                    },
                    NewEpisode {
                        episode_number: 2,
                        title: None,
                        air_date: None,
                    },
                ],
            }],
        )
        .await
        .unwrap();
        // The provider FK is real, so insert through the same path the
        // app uses rather than hand-rolling the INSERT.
        let base_url = url::Url::parse("https://capybarabr.com/").unwrap();
        let provider = crate::db::providers::insert(
            pool,
            crate::db::providers::NewProvider {
                name: "capybara",
                base_url: &base_url,
                api_token: "tok",
                kind: "unit3d",
                plugin_path: None,
            },
        )
        .await
        .unwrap();
        (item.id, provider.id)
    }

    /// Build the shape a pre-fix database carries: a grab taken **for an
    /// episode**, imported, whose `episode_id` a metadata refresh then
    /// nulled. The `UPDATE` stands in for the `ON DELETE SET NULL` that
    /// `sync_seasons` used to trigger — no code path produces this any
    /// more, which is exactly why the fixture has to forge it.
    async fn orphan(pool: &Pool, item_id: Uuid, provider_id: Uuid, guid: &str, path: &str) -> Uuid {
        let episode = library::episodes(pool, item_id).await.unwrap()[0].id;
        let grab = reserve(
            pool,
            &NewGrab {
                item_id,
                episode_id: Some(episode),
                season_number: None,
                decision_id: None,
                provider_id,
                provider_name: "capybara",
                release_id_remote: guid,
                release_name: guid,
                download_url: None,
                protocol: crate::db::grabs::Protocol::Torrent,
            },
        )
        .await
        .unwrap()
        .unwrap();
        set_status(pool, grab.id, GrabStatus::Completed, None)
            .await
            .unwrap();
        mark_imported(pool, grab.id, path).await.unwrap();
        sqlx::query("UPDATE grabs SET episode_id = NULL WHERE id = ?")
            .bind(grab.id.to_string())
            .execute(pool)
            .await
            .unwrap();
        grab.id
    }

    #[tokio::test]
    async fn a_marked_file_name_points_the_grab_back_at_its_episode() {
        let pool = open_memory().await.unwrap();
        let (item_id, provider_id) = series(&pool).await;
        let id = orphan(
            &pool,
            item_id,
            provider_id,
            "a",
            "/midias/DBS/Season 01/Dragon Ball Super - S01E02.mkv",
        )
        .await;

        let report = run(&pool).await.unwrap();

        assert_eq!(report.examined, 1);
        assert_eq!(report.linked, 1);
        let eps = library::episodes(&pool, item_id).await.unwrap();
        let target = eps.iter().find(|e| e.episode_number == 2).unwrap();
        assert_eq!(
            grabs::get_by_id(&pool, id).await.unwrap().episode_id,
            Some(target.id)
        );
    }

    #[tokio::test]
    async fn an_absolute_numbered_name_is_left_alone() {
        // The 224 Yu-Gi-Oh! files carry no marker at all. Guessing an
        // episode from a bare number is exactly the wrong link this
        // module refuses to make — the *arr path answers these.
        let pool = open_memory().await.unwrap();
        let (item_id, provider_id) = series(&pool).await;
        let id = orphan(
            &pool,
            item_id,
            provider_id,
            "a",
            "/midias/Animes/Dragon Ball Super - 131.mkv",
        )
        .await;

        let report = run(&pool).await.unwrap();

        assert_eq!(report.no_marker, 1);
        assert_eq!(report.linked, 0);
        assert!(
            grabs::get_by_id(&pool, id)
                .await
                .unwrap()
                .episode_id
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_marker_the_catalogue_does_not_have_is_reported_not_guessed() {
        let pool = open_memory().await.unwrap();
        let (item_id, provider_id) = series(&pool).await;
        orphan(
            &pool,
            item_id,
            provider_id,
            "a",
            "/midias/DBS/Dragon Ball Super - S09E99.mkv",
        )
        .await;

        let report = run(&pool).await.unwrap();

        assert_eq!(report.unknown_episode, 1);
        assert_eq!(report.linked, 0);
    }

    #[tokio::test]
    async fn a_second_pass_changes_nothing() {
        let pool = open_memory().await.unwrap();
        let (item_id, provider_id) = series(&pool).await;
        orphan(
            &pool,
            item_id,
            provider_id,
            "a",
            "/midias/DBS/Dragon Ball Super - S01E01.mkv",
        )
        .await;

        assert_eq!(run(&pool).await.unwrap().linked, 1);
        let second = run(&pool).await.unwrap();
        assert_eq!(second.examined, 0, "a repaired grab is no longer an orphan");
        assert_eq!(second.linked, 0);
    }

    #[tokio::test]
    async fn a_movie_grab_is_not_an_orphan() {
        // Naming no episode is the *correct* shape for a film, and
        // `scope` is what tells the two apart — before the column the
        // query had to infer it from the item's media type.
        let pool = open_memory().await.unwrap();
        let (_, provider_id) = series(&pool).await;
        let movie = upsert(
            &pool,
            &NewLibraryItem {
                media_type: Some(MediaType::Movie),
                tmdb_id: 603,
                title: "The Matrix".to_owned(),
                ..NewLibraryItem::default()
            },
        )
        .await
        .unwrap();
        let grab = reserve(
            &pool,
            &NewGrab {
                item_id: movie.id,
                episode_id: None,
                season_number: None,
                decision_id: None,
                provider_id,
                provider_name: "capybara",
                release_id_remote: "m",
                release_name: "m",
                download_url: None,
                protocol: crate::db::grabs::Protocol::Torrent,
            },
        )
        .await
        .unwrap()
        .unwrap();
        set_status(&pool, grab.id, GrabStatus::Completed, None)
            .await
            .unwrap();
        mark_imported(&pool, grab.id, "/midias/Filmes/The Matrix (1999).mkv")
            .await
            .unwrap();

        assert_eq!(run(&pool).await.unwrap().examined, 0);
    }
}
