//! Bridge between [`brarr_tmdb`] and the library tables.
//!
//! Two responsibilities, kept apart on purpose:
//!
//! - **Pure conversion** ([`movie_to_item`], [`tv_to_item`],
//!   [`season_to_new`]) — TMDB record in, `db::library` input out. No
//!   pool, no network, so the mapping is unit-testable on its own.
//! - **Orchestration** ([`add_movie`], [`add_series`], [`refresh`]) —
//!   fetch, convert, persist.
//!
//! The division of ownership from `db::library` holds here too: these
//! functions only ever write TMDB-owned columns. Monitoring, profile and
//! root folder belong to the operator and are set elsewhere.

use brarr_tmdb::{MovieDetails, SeasonDetails, TmdbClient, TmdbError, TvDetails};
use time::{Date, OffsetDateTime, Time};
use uuid::Uuid;

use crate::{
    AppError,
    db::{
        Pool,
        library::{self, LibraryItem, MediaType, NewEpisode, NewLibraryItem, NewSeason},
        settings,
    },
    structure,
};

/// Default metadata staleness before a refresh, in days. Well inside the
/// six-month ceiling the TMDB terms impose on cached metadata.
pub const DEFAULT_TTL_DAYS: i64 = 30;

/// Environment fallback for the read access token.
const ENV_TOKEN: &str = "BRARR_TMDB_TOKEN";

impl From<TmdbError> for AppError {
    fn from(err: TmdbError) -> Self {
        match err {
            TmdbError::NotFound(what) => Self::NotFound(format!("TMDB: {what}")),
            TmdbError::Unauthorized => Self::InvalidInput(
                "TMDB recusou as credenciais — confira o read access token (v4) em /settings"
                    .to_owned(),
            ),
            other => Self::Tmdb(other),
        }
    }
}

/// Midnight UTC of a calendar date. TMDB deals in dates; the library
/// columns are epoch seconds.
fn at_midnight(date: Date) -> OffsetDateTime {
    OffsetDateTime::new_utc(date, Time::MIDNIGHT)
}

/// Force the canonical `ttNNNNNNN` form.
///
/// TMDB already returns the prefix, but `searches.imdb_id` stores the
/// bare number and ids arriving from that side would otherwise be
/// written into the library in the wrong convention.
#[must_use]
pub fn canonical_imdb(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with("tt") {
        Some(trimmed.to_owned())
    } else if trimmed.chars().all(|c| c.is_ascii_digit()) {
        // Bare number from the `searches` convention — pad to the 7
        // digits IMDb uses so both forms land on the same string.
        Some(format!("tt{trimmed:0>7}"))
    } else {
        None
    }
}

/// TMDB movie → library row input.
#[must_use]
pub fn movie_to_item(details: &MovieDetails) -> NewLibraryItem {
    NewLibraryItem {
        media_type: Some(MediaType::Movie),
        tmdb_id: details.tmdb_id,
        imdb_id: details.imdb_id.as_deref().and_then(canonical_imdb),
        // TMDB has no tvdb mapping for movies; leaving this Some would be
        // a lie the UI would happily render.
        tvdb_id: None,
        title: details.title.clone(),
        original_title: details.original_title.clone(),
        year: details.release_date.map(Date::year),
        overview: details.overview.clone(),
        poster_path: details.poster_path.clone(),
        backdrop_path: details.backdrop_path.clone(),
        tmdb_status: details.status.clone(),
        runtime_minutes: details.runtime_minutes,
        next_air_date: None,
        digital_release_at: details.digital_release.map(at_midnight),
        physical_release_at: details.physical_release.map(at_midnight),
    }
}

/// TMDB series → library row input.
#[must_use]
pub fn tv_to_item(details: &TvDetails) -> NewLibraryItem {
    NewLibraryItem {
        media_type: Some(MediaType::Tv),
        tmdb_id: details.tmdb_id,
        imdb_id: details.imdb_id.as_deref().and_then(canonical_imdb),
        tvdb_id: details.tvdb_id,
        title: details.name.clone(),
        original_title: details.original_name.clone(),
        year: details.first_air_date.map(Date::year),
        overview: details.overview.clone(),
        poster_path: details.poster_path.clone(),
        backdrop_path: details.backdrop_path.clone(),
        tmdb_status: details.status.clone(),
        runtime_minutes: details.episode_runtime,
        next_air_date: details.next_air_date.map(at_midnight),
        digital_release_at: None,
        physical_release_at: None,
    }
}

/// TMDB season → library season input.
#[must_use]
pub fn season_to_new(season: &SeasonDetails) -> NewSeason {
    NewSeason {
        season_number: season.season_number,
        // Trust the episode list over any count field: the list is what
        // the tree is actually built from.
        episode_count: i32::try_from(season.episodes.len()).unwrap_or(0),
        air_date: season.air_date.map(at_midnight),
        episodes: season
            .episodes
            .iter()
            .map(|e| NewEpisode {
                tmdb_episode_id: (e.id > 0).then_some(e.id),
                episode_number: e.episode_number,
                title: e.title.clone(),
                air_date: e.air_date.map(at_midnight),
            })
            .collect(),
    }
}

/// Resolved TMDB configuration.
#[derive(Debug, Clone)]
pub struct TmdbConfig {
    /// Read access token. Empty means "not configured".
    pub token: String,
    /// Metadata language.
    pub language: String,
    /// Country for release-date resolution.
    pub country: String,
    /// Refresh staleness threshold in days.
    pub ttl_days: i64,
}

impl TmdbConfig {
    /// Whether a client can be built at all.
    #[must_use]
    pub fn is_configured(&self) -> bool {
        !self.token.trim().is_empty()
    }
}

/// Read the TMDB configuration: settings row first, environment second.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn load_config(pool: &Pool) -> Result<TmdbConfig, AppError> {
    let stored = settings::get_all(pool).await?;
    let pick = |key: &str| -> Option<String> {
        stored
            .get(key)
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    };
    let token = pick(settings::KEY_TMDB_TOKEN)
        .or_else(|| std::env::var(ENV_TOKEN).ok())
        .unwrap_or_default();
    let ttl_days = pick(settings::KEY_TMDB_TTL_DAYS)
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|d| *d > 0)
        .unwrap_or(DEFAULT_TTL_DAYS);
    Ok(TmdbConfig {
        token: token.trim().to_owned(),
        language: pick(settings::KEY_TMDB_LANGUAGE).unwrap_or_else(|| "pt-BR".to_owned()),
        country: pick(settings::KEY_TMDB_COUNTRY).unwrap_or_else(|| "BR".to_owned()),
        ttl_days,
    })
}

/// Build a client from persisted configuration.
///
/// # Errors
///
/// Returns [`AppError::InvalidInput`] when no token is configured, and
/// propagates client construction failures.
pub async fn client(pool: &Pool) -> Result<TmdbClient, AppError> {
    let cfg = load_config(pool).await?;
    if !cfg.is_configured() {
        return Err(AppError::InvalidInput(
            "TMDB não configurado — informe o read access token em /settings".to_owned(),
        ));
    }
    Ok(TmdbClient::new(&cfg.token)?
        .with_language(&cfg.language)
        .with_country(&cfg.country))
}

/// Add a movie to the library, or refresh it if already present.
///
/// # Errors
///
/// Propagates TMDB and database failures.
pub async fn add_movie(
    pool: &Pool,
    tmdb: &TmdbClient,
    tmdb_id: i64,
) -> Result<LibraryItem, AppError> {
    let details = tmdb.movie(tmdb_id).await?;
    let item = library::upsert(pool, &movie_to_item(&details)).await?;
    Ok(item)
}

/// Add a series to the library, or refresh it if already present, then
/// build its season tree from whoever owns its shape.
///
/// **The description and the shape have different owners**, and this is
/// where that starts. Title, synopsis and artwork come from TMDB and
/// stay TMDB's; the tree comes from [`crate::metadata::owned::tree`],
/// which for a series nobody has claimed yet decides — and decides
/// TheTVDB when it can, because that is the numbering releases use.
///
/// # Errors
///
/// Propagates TMDB, provider and database failures.
pub async fn add_series(
    pool: &Pool,
    tmdb: &TmdbClient,
    registry: &crate::metadata::registry::Registry,
    tmdb_id: i64,
) -> Result<LibraryItem, AppError> {
    let details = tmdb.tv(tmdb_id).await?;
    let item = library::upsert(pool, &tv_to_item(&details)).await?;
    let tree = crate::metadata::owned::tree(pool, registry, item.id).await?;
    structure::apply(pool, item.id, &tree).await?;
    Ok(item)
}

/// What the operator chose in the add dialog.
///
/// Every field is optional and `None` means "leave whatever is there".
/// That matters because adding a title that is *already* in the
/// catalogue must not quietly wipe the placement the operator set by
/// hand — [`library::set_placement`] writes both of its columns
/// unconditionally.
#[derive(Debug, Clone, Default)]
pub struct AddOptions {
    /// Quality profile to score against.
    pub profile_id: Option<Uuid>,
    /// Destination root folder, already validated against the
    /// registered list by the caller.
    pub root_folder: Option<String>,
    /// How much of the title to chase.
    pub monitor_scope: Option<library::MonitorScope>,
}

/// Add a title with the operator's choices applied, in the one order
/// that works.
///
/// The sequence is not incidental:
///
/// 1. **Scope first**, because [`library::sync_seasons`] reads it to
///    decide the default for every season and episode row it is about
///    to create. Setting it afterwards would rebuild the tree under the
///    old scope and then quietly disagree with the screen.
/// 2. **Then the metadata walk**, which builds the tree.
/// 3. **Then placement**, which is independent of both.
///
/// # Errors
///
/// Propagates TMDB and database failures.
pub async fn add_with_options(
    pool: &Pool,
    tmdb: &TmdbClient,
    registry: &crate::metadata::registry::Registry,
    media_type: MediaType,
    tmdb_id: i64,
    options: &AddOptions,
) -> Result<LibraryItem, AppError> {
    // The row has to exist before its scope can be set, and `upsert`
    // preserves operator state on conflict, so this is safe for a title
    // that is already catalogued.
    let item = match media_type {
        MediaType::Movie => {
            let details = tmdb.movie(tmdb_id).await?;
            library::upsert(pool, &movie_to_item(&details)).await?
        }
        MediaType::Tv => {
            let details = tmdb.tv(tmdb_id).await?;
            library::upsert(pool, &tv_to_item(&details)).await?
        }
    };

    if let Some(scope) = options.monitor_scope {
        library::set_monitor_scope(pool, item.id, scope).await?;
    }

    if media_type == MediaType::Tv {
        let tree = crate::metadata::owned::tree(pool, registry, item.id).await?;
        structure::apply(pool, item.id, &tree).await?;
    }

    // Only touch placement when the operator actually chose something.
    // Calling `set_placement` with two `None`s would erase a profile and
    // a root folder that a previous add — or the detail screen — set.
    if options.profile_id.is_some() || options.root_folder.is_some() {
        library::set_placement(
            pool,
            item.id,
            options.profile_id.or(item.profile_id),
            options
                .root_folder
                .as_deref()
                .or(item.root_folder.as_deref()),
        )
        .await?;
    }

    library::get_by_id(pool, item.id).await
}

/// Refresh one library item's metadata from TMDB, rebuilding the season
/// tree for series.
///
/// # Errors
///
/// Propagates TMDB and database failures.
pub async fn refresh(
    pool: &Pool,
    tmdb: &TmdbClient,
    registry: &crate::metadata::registry::Registry,
    item_id: Uuid,
) -> Result<LibraryItem, AppError> {
    let item = library::get_by_id(pool, item_id).await?;
    match item.media_type {
        MediaType::Movie => add_movie(pool, tmdb, item.tmdb_id).await,
        // The two facets have two owners, and a refresh has to ask each
        // of them its own question. Title, synopsis and artwork are
        // TMDB's and stay TMDB's; the shape belongs to whoever the item
        // records, which for a flipped title is not TMDB — and handing
        // it TMDB's tree anyway is a write the source gate refuses, so
        // the title would simply stop refreshing.
        MediaType::Tv => {
            let details = tmdb.tv(item.tmdb_id).await?;
            let refreshed = library::upsert(pool, &tv_to_item(&details)).await?;
            let tree = crate::metadata::owned::tree(pool, registry, refreshed.id).await?;
            structure::apply(pool, refreshed.id, &tree).await?;
            Ok(refreshed)
        }
    }
}

/// Items whose metadata is older than the configured TTL, oldest first.
///
/// # Errors
///
/// Returns [`AppError::Database`] on SQL failure.
pub async fn stale(pool: &Pool, ttl_days: i64) -> Result<Vec<LibraryItem>, AppError> {
    let cutoff = OffsetDateTime::now_utc() - time::Duration::days(ttl_days.max(1));
    let all = library::list(pool).await?;
    Ok(all
        .into_iter()
        .filter(|i| i.metadata_refreshed_at < cutoff)
        .collect())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert on happy paths"
)]
mod tests {
    use super::*;
    use brarr_tmdb::{Episode, SeasonSummary};
    use time::macros::date;

    fn movie() -> MovieDetails {
        MovieDetails {
            tmdb_id: 603,
            imdb_id: Some("tt0133093".to_owned()),
            title: "Matrix".to_owned(),
            original_title: Some("The Matrix".to_owned()),
            overview: Some("Um hacker descobre…".to_owned()),
            poster_path: Some("/p.jpg".to_owned()),
            backdrop_path: None,
            release_date: Some(date!(1999 - 03 - 30)),
            digital_release: Some(date!(1999 - 11 - 05)),
            physical_release: Some(date!(2000 - 02 - 18)),
            runtime_minutes: Some(136),
            status: Some("Released".to_owned()),
        }
    }

    fn series() -> TvDetails {
        TvDetails {
            tmdb_id: 76479,
            imdb_id: Some("tt1190634".to_owned()),
            tvdb_id: Some(355_567),
            name: "The Boys".to_owned(),
            original_name: Some("The Boys".to_owned()),
            overview: Some("Vigilantes…".to_owned()),
            poster_path: Some("/b.jpg".to_owned()),
            backdrop_path: None,
            first_air_date: Some(date!(2019 - 07 - 25)),
            status: Some("Returning Series".to_owned()),
            in_production: true,
            next_air_date: Some(date!(2026 - 08 - 12)),
            episode_runtime: Some(60),
            seasons: vec![SeasonSummary {
                season_number: 1,
                episode_count: 8,
                air_date: Some(date!(2019 - 07 - 25)),
            }],
        }
    }

    #[test]
    fn movie_maps_dates_and_ids() {
        let item = movie_to_item(&movie());
        assert_eq!(item.media_type, Some(MediaType::Movie));
        assert_eq!(item.tmdb_id, 603);
        assert_eq!(item.imdb_id.as_deref(), Some("tt0133093"));
        assert_eq!(item.year, Some(1999));
        assert_eq!(item.runtime_minutes, Some(136));
        assert_eq!(
            item.digital_release_at.map(OffsetDateTime::date),
            Some(date!(1999 - 11 - 05))
        );
    }

    #[test]
    fn a_movie_never_carries_a_tvdb_id() {
        // TMDB has no tvdb mapping for movies; inventing one would put a
        // wrong chip on the detail screen.
        assert_eq!(movie_to_item(&movie()).tvdb_id, None);
    }

    #[test]
    fn series_maps_tvdb_id_and_next_air_date() {
        let item = tv_to_item(&series());
        assert_eq!(item.media_type, Some(MediaType::Tv));
        assert_eq!(item.tvdb_id, Some(355_567));
        assert_eq!(item.title, "The Boys");
        assert_eq!(item.year, Some(2019));
        assert_eq!(
            item.next_air_date.map(OffsetDateTime::date),
            Some(date!(2026 - 08 - 12))
        );
        assert_eq!(
            item.digital_release_at, None,
            "release dates are movie-only"
        );
    }

    #[test]
    fn canonical_imdb_accepts_both_conventions() {
        assert_eq!(canonical_imdb("tt0133093").as_deref(), Some("tt0133093"));
        // `searches.imdb_id` stores the bare number; the library stores
        // the prefixed form, and both have to land on the same string.
        assert_eq!(canonical_imdb("133093").as_deref(), Some("tt0133093"));
        assert_eq!(canonical_imdb("0133093").as_deref(), Some("tt0133093"));
        assert_eq!(canonical_imdb(""), None);
        assert_eq!(canonical_imdb("   "), None);
        assert_eq!(canonical_imdb("nao-e-um-id"), None);
    }

    #[test]
    fn season_episode_count_follows_the_actual_list() {
        let season = SeasonDetails {
            season_number: 4,
            air_date: Some(date!(2024 - 06 - 13)),
            episodes: vec![
                Episode {
                    id: 3_910_571,
                    season_number: 4,
                    episode_number: 1,
                    title: Some("A".to_owned()),
                    air_date: Some(date!(2024 - 06 - 13)),
                },
                Episode {
                    id: 3_910_572,
                    season_number: 4,
                    episode_number: 2,
                    title: None,
                    air_date: None,
                },
            ],
        };
        let mapped = season_to_new(&season);
        assert_eq!(mapped.season_number, 4);
        assert_eq!(mapped.episode_count, 2);
        assert_eq!(mapped.episodes.len(), 2);
        assert_eq!(
            mapped.episodes[1].air_date, None,
            "an unaired episode keeps its slot with no date"
        );
    }

    #[tokio::test]
    async fn config_defaults_when_nothing_is_stored() {
        let pool = crate::db::open_memory().await.unwrap();
        let cfg = load_config(&pool).await.unwrap();
        assert_eq!(cfg.language, "pt-BR");
        assert_eq!(cfg.country, "BR");
        assert_eq!(cfg.ttl_days, DEFAULT_TTL_DAYS);
    }

    #[tokio::test]
    async fn settings_override_the_defaults() {
        let pool = crate::db::open_memory().await.unwrap();
        settings::set(&pool, settings::KEY_TMDB_LANGUAGE, "en-US")
            .await
            .unwrap();
        settings::set(&pool, settings::KEY_TMDB_COUNTRY, "US")
            .await
            .unwrap();
        settings::set(&pool, settings::KEY_TMDB_TTL_DAYS, "7")
            .await
            .unwrap();
        let cfg = load_config(&pool).await.unwrap();
        assert_eq!(cfg.language, "en-US");
        assert_eq!(cfg.country, "US");
        assert_eq!(cfg.ttl_days, 7);
    }

    #[tokio::test]
    async fn a_blanked_setting_falls_back_rather_than_becoming_empty() {
        let pool = crate::db::open_memory().await.unwrap();
        // The settings UI persists "" to mean "cleared"; that must read
        // as "use the default", not as an empty language code.
        settings::set(&pool, settings::KEY_TMDB_LANGUAGE, "")
            .await
            .unwrap();
        settings::set(&pool, settings::KEY_TMDB_TTL_DAYS, "0")
            .await
            .unwrap();
        let cfg = load_config(&pool).await.unwrap();
        assert_eq!(cfg.language, "pt-BR");
        assert_eq!(
            cfg.ttl_days, DEFAULT_TTL_DAYS,
            "0 days would refresh forever"
        );
    }

    #[tokio::test]
    async fn client_refuses_to_build_without_a_token() {
        let pool = crate::db::open_memory().await.unwrap();
        // Guard against a token leaking in from the developer's own env.
        if std::env::var(ENV_TOKEN).is_ok() {
            return;
        }
        let err = client(&pool).await.unwrap_err();
        assert!(matches!(err, AppError::InvalidInput(_)));
    }

    /// End-to-end smoke test against the real TMDB. Ignored by default
    /// because it needs a credential and network; run it with
    ///
    /// ```text
    /// BRARR_TMDB_TOKEN=<key> cargo test -p brarr-orchestrator -- --ignored tmdb_live
    /// ```
    ///
    /// Accepts either credential shape — the client detects whether it
    /// holds a v4 read access token or a v3 API key.
    #[tokio::test]
    #[ignore = "hits the live TMDB API; needs BRARR_TMDB_TOKEN"]
    async fn tmdb_live_add_movie_and_series() {
        let pool = crate::db::open_memory().await.unwrap();
        let tmdb = client(&pool)
            .await
            .expect("BRARR_TMDB_TOKEN must be set for the live test");

        tmdb.verify_token().await.expect("credential must work");

        let matrix = add_movie(&pool, &tmdb, 603).await.unwrap();
        assert_eq!(matrix.tmdb_id, 603);
        assert_eq!(matrix.imdb_id.as_deref(), Some("tt0133093"));
        assert!(!matrix.title.is_empty());

        // No TheTVDB credential in this test, so the registry offers
        // only TMDB and the series is born under it — which is exactly
        // what a deployment with one credential should do.
        let registry = crate::metadata::registry::Registry::build(&pool)
            .await
            .unwrap();
        let boys = add_series(&pool, &tmdb, &registry, 76_479).await.unwrap();
        assert_eq!(boys.tvdb_id, Some(355_567));
        let seasons = library::seasons(&pool, boys.id).await.unwrap();
        let episodes = library::episodes(&pool, boys.id).await.unwrap();
        assert!(seasons.len() >= 5, "got {} seasons", seasons.len());
        assert!(
            episodes.len() > 30,
            "the whole episode tree should land, got {}",
            episodes.len()
        );
    }

    #[tokio::test]
    async fn stale_lists_only_items_past_the_ttl() {
        let pool = crate::db::open_memory().await.unwrap();
        library::upsert(&pool, &movie_to_item(&movie()))
            .await
            .unwrap();
        // Freshly written, so nothing is stale yet.
        assert!(stale(&pool, 30).await.unwrap().is_empty());
    }
}
