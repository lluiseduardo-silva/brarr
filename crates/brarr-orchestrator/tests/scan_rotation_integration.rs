//! The sweep's queue, through the real `run_once`.
//!
//! Two defects measured on this operator's live database, both of which
//! rendered as "brarr just never looked for that episode":
//!
//! 1. The per-cycle ceiling was spent from a **fixed** head — items by
//!    `metadata_refreshed_at`, then season and episode — so the same
//!    targets were searched every cycle forever and the rest never once.
//!    294 wanted targets against a ceiling of 25, and the head of that
//!    list is by construction the part that never finds anything: a
//!    target is wanted *because* nothing was found for it.
//! 2. The film path had no release gate at all, so a sequel still being
//!    shot was asked of every configured tracker every thirty minutes —
//!    while `coverage` painted it "não estreou" on the shelf.
//!
//! `run_once` is driven directly rather than through `run_one_cycle`: no
//! provider is configured, so every search fans out over nothing and
//! returns immediately. Nothing here touches the network.

// `clippy::doc_markdown` for the shared `support` module, the same way
// every other test binary that includes it does.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::doc_markdown)]

mod support;

use std::net::SocketAddr;
use std::time::Duration;

use brarr_decision_service::Engine;
use brarr_orchestrator::db::library::{self, MediaType, NewEpisode, NewSeason, ProductionStatus};
use brarr_orchestrator::db::{scan_attempts, settings};
use brarr_orchestrator::{AppState, db, scan, web};
use time::OffsetDateTime;
use uuid::Uuid;

fn days_ago(n: i64) -> OffsetDateTime {
    OffsetDateTime::now_utc() - time::Duration::days(n)
}

async fn state() -> AppState {
    let pool = db::open_memory().await.expect("open in-memory db");
    AppState::new(pool, Engine::baseline())
}

/// The shape measured in production, one size down: a season whose gaps
/// can never close — the operator's files are on disk under a different
/// cut of the show, so no release will ever carry those episode numbers —
/// sitting ahead of the episode that aired two days ago.
async fn seed_a_backlog_and_one_fresh_episode(state: &AppState) -> Uuid {
    let item = library::upsert(
        state.pool(),
        &support::Seed::series(94_664, "Re:ZERO")
            .year(2016)
            .tvdb(305_089)
            .build(),
    )
    .await
    .unwrap();

    let backlog: Vec<NewEpisode> = (14..=25)
        .map(|n| NewEpisode {
            air_date: Some(days_ago(3500)),
            ..support::episode(n)
        })
        .collect();

    support::tree(
        state.pool(),
        item.id,
        &[
            NewSeason {
                season_number: 1,
                episode_count: 12,
                air_date: Some(days_ago(3500)),
                episodes: backlog,
            },
            NewSeason {
                season_number: 4,
                episode_count: 1,
                air_date: Some(days_ago(2)),
                episodes: vec![NewEpisode {
                    air_date: Some(days_ago(2)),
                    ..support::episode(12)
                }],
            },
        ],
    )
    .await;
    item.id
}

/// Which targets a cycle actually searched, as coordinates.
async fn searched(state: &AppState, item: Uuid) -> Vec<(i32, i32)> {
    let attempts = scan_attempts::last_searched(state.pool()).await.unwrap();
    let episodes = library::episodes(state.pool(), item).await.unwrap();
    let mut out: Vec<(i32, i32)> = episodes
        .iter()
        .filter(|e| attempts.contains_key(&(item, Some(e.id))))
        .map(|e| (e.season_number, e.episode_number))
        .collect();
    out.sort_unstable();
    out
}

/// The regression. Under the old fixed order this episode was never
/// searched once, in any cycle, no matter how long brarr ran.
#[tokio::test]
async fn a_freshly_aired_episode_is_searched_ahead_of_a_backlog_that_outgrew_the_ceiling() {
    let state = state().await;
    let item = seed_a_backlog_and_one_fresh_episode(&state).await;
    settings::set(state.pool(), settings::KEY_SCAN_SEARCHES_PER_CYCLE, "3")
        .await
        .unwrap();

    let summary = scan::run_once(&state).await.unwrap();

    assert_eq!(summary.targets, 13, "twelve gaps plus the new episode");
    assert_eq!(summary.searches, 3, "the ceiling holds");
    assert_eq!(summary.skipped_over_cap, 10);
    assert!(
        searched(&state, item).await.contains(&(4, 12)),
        "the episode that aired two days ago has to be in the first cycle, \
         not behind twelve gaps that will never close"
    );
}

/// The ceiling is a delay, not a wall. A second cycle has to reach
/// targets the first one could not — that is the whole difference
/// between "picked up next cycle" and "never".
///
/// The fresh episode is legitimately searched again: it is the one thing
/// being waited for, and releases for it appear by the hour. What has to
/// move is the backlog slice.
#[tokio::test]
async fn the_next_cycle_reaches_targets_the_last_one_could_not() {
    let state = state().await;
    let item = seed_a_backlog_and_one_fresh_episode(&state).await;
    settings::set(state.pool(), settings::KEY_SCAN_SEARCHES_PER_CYCLE, "3")
        .await
        .unwrap();

    scan::run_once(&state).await.unwrap();
    let first = searched(&state, item).await;
    assert_eq!(first.len(), 3);
    assert!(first.contains(&(4, 12)));

    scan::run_once(&state).await.unwrap();
    let second = searched(&state, item).await;

    assert!(
        first.iter().all(|t| second.contains(t)),
        "it advances rather than restarting: {first:?} ⊄ {second:?}"
    );
    let fresh_backlog = second.iter().filter(|t| !first.contains(t)).count();
    assert_eq!(
        fresh_backlog, 2,
        "the two slots not held by the fresh episode moved on: {second:?}"
    );
}

/// **A share is a floor, never a priority.** Measured before this
/// existed: 30 targets inside the two-week window against a ceiling of
/// 25, so ordering fresh ahead of backlog would have spent every cycle
/// inside the window and left 249 targets at zero searches — the same
/// starvation, one tier down, and invisible because the sweep looks busy.
#[tokio::test]
async fn a_fresh_tier_larger_than_the_ceiling_still_leaves_the_backlog_a_share() {
    let state = state().await;
    let item = library::upsert(
        state.pool(),
        &support::Seed::series(1, "Weekly")
            .year(2026)
            .tvdb(9_001)
            .build(),
    )
    .await
    .unwrap();

    // Thirty episodes out inside the window, forty long overdue.
    let fresh: Vec<NewEpisode> = (1..=30)
        .map(|n| NewEpisode {
            air_date: Some(days_ago(3)),
            ..support::episode(n)
        })
        .collect();
    let backlog: Vec<NewEpisode> = (1..=40)
        .map(|n| NewEpisode {
            air_date: Some(days_ago(2000)),
            ..support::episode(n)
        })
        .collect();
    support::tree(
        state.pool(),
        item.id,
        &[
            NewSeason {
                season_number: 1,
                episode_count: 40,
                air_date: Some(days_ago(2000)),
                episodes: backlog,
            },
            NewSeason {
                season_number: 2,
                episode_count: 30,
                air_date: Some(days_ago(3)),
                episodes: fresh,
            },
        ],
    )
    .await;
    settings::set(state.pool(), settings::KEY_SCAN_SEARCHES_PER_CYCLE, "25")
        .await
        .unwrap();

    let summary = scan::run_once(&state).await.unwrap();
    assert_eq!(summary.searches, 25, "the ceiling still holds");

    let hit = searched(&state, item.id).await;
    let old = hit.iter().filter(|(season, _)| *season == 1).count();
    let new = hit.iter().filter(|(season, _)| *season == 2).count();
    assert_eq!(new, 17, "the fresh tier takes its share and no more");
    assert_eq!(old, 8, "and the backlog is not left at zero");
}

/// The other direction: a quiet week has nothing fresh, and the backlog
/// should get the whole cycle rather than 70% of it. A share that acted
/// as a cap would leave slots unspent every time nothing aired.
#[tokio::test]
async fn a_quiet_week_gives_the_whole_cycle_to_the_backlog() {
    let state = state().await;
    let item = seed_a_backlog_and_one_fresh_episode(&state).await;
    // Twelve backlog gaps, one fresh episode, and room for five.
    settings::set(state.pool(), settings::KEY_SCAN_SEARCHES_PER_CYCLE, "5")
        .await
        .unwrap();

    let summary = scan::run_once(&state).await.unwrap();

    assert_eq!(summary.searches, 5, "every slot is spent");
    let hit = searched(&state, item).await;
    assert_eq!(
        hit.iter().filter(|(season, _)| *season == 1).count(),
        4,
        "the fresh tier holds one target, so the other four go to the backlog"
    );
}

/// A film still being shot has no release to find, and no date to reason
/// about either — so a date test alone would read "no date ⇒ out" and
/// send the sweep after it every cycle. Two of this operator's seventeen
/// wanted films are exactly this.
#[tokio::test]
async fn a_film_still_in_production_is_not_a_target() {
    let state = state().await;
    let unreleased = library::upsert(
        state.pool(),
        &support::Seed::movie(1_234_821, "Homem-Aranha: Além do Aranhaverso")
            .year(2027)
            .with(|item| item.status = Some(ProductionStatus::InProduction))
            .build(),
    )
    .await
    .unwrap();
    assert_eq!(unreleased.media_type, MediaType::Movie);

    let summary = scan::run_once(&state).await.unwrap();

    assert_eq!(summary.targets, 0, "nothing to search for");
    assert_eq!(summary.searches, 0, "and so no tracker was asked");
    assert_eq!(
        summary.skipped_unreleased, 1,
        "counted, not silently dropped"
    );
}

/// The other half of the gate: a film whose digital release has not
/// arrived. The theatrical window is what the detail page already warns
/// about — the sweep was chasing it anyway.
#[tokio::test]
async fn a_film_whose_digital_release_has_not_arrived_waits() {
    let state = state().await;
    library::upsert(
        state.pool(),
        &support::Seed::movie(1_022_789, "Toy Story 5")
            .year(2026)
            .with(|item| {
                item.status = Some(ProductionStatus::Released);
                item.digital_release_at = Some(OffsetDateTime::now_utc() + time::Duration::days(4));
            })
            .build(),
    )
    .await
    .unwrap();

    let summary = scan::run_once(&state).await.unwrap();
    assert_eq!(summary.skipped_unreleased, 1);
    assert_eq!(summary.searches, 0);
}

/// The ceiling is reachable from the screen, end to end: rendered with
/// the value the sweep is actually using, saved, and read back by the
/// sweep without a restart.
///
/// Through the real router because the wiring is the part that breaks —
/// a field can exist in the form struct, be parsed, and never be
/// rendered, and nothing in a unit test would notice.
#[tokio::test]
async fn the_ceiling_round_trips_through_the_settings_screen() {
    let state = state().await;
    let static_dir = std::env::temp_dir().join("brarr-scan-rotation-static");
    let _ = tokio::fs::create_dir_all(&static_dir).await;
    let router = web::router(state.clone(), &static_dir);
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let page = reqwest::get(format!("http://{addr}/settings?s=automacao"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        page.contains(r#"name="scan_searches_per_cycle""#),
        "the field has to be on the screen, not only in the form struct"
    );
    assert!(
        page.contains(&format!(r#"value="{}""#, scan::DEFAULT_SEARCHES_PER_CYCLE)),
        "and pre-filled with what the sweep is using, not an empty box"
    );

    let response = reqwest::Client::new()
        .post(format!("http://{addr}/settings/general"))
        .form(&[("scan_searches_per_cycle", "40")])
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());

    let stored = settings::get(state.pool(), settings::KEY_SCAN_SEARCHES_PER_CYCLE)
        .await
        .unwrap()
        .expect("saved");
    assert_eq!(stored.value, "40");
}

/// And the default that keeps a back catalogue searchable: TMDB carries
/// no digital date for most older films, so "no date" cannot mean "not
/// out" — it is the same default `coverage::movie_progress` documents.
#[tokio::test]
async fn an_older_film_with_no_digital_date_is_still_searched() {
    let state = state().await;
    library::upsert(
        state.pool(),
        &support::Seed::movie(603, "The Matrix")
            .year(1999)
            .with(|item| item.status = Some(ProductionStatus::Released))
            .build(),
    )
    .await
    .unwrap();

    let summary = scan::run_once(&state).await.unwrap();
    assert_eq!(summary.skipped_unreleased, 0);
    assert_eq!(summary.searches, 1, "one target, and it was searched");
}
