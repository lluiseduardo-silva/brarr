//! Integration tests for the library status surface.
//!
//! The screens make four claims, and each one is a place a wrong answer
//! looks plausible: the denominator is what is *monitored*, an unaired
//! episode is not a gap, a season toggle really did move every episode
//! under it, and the tooltip names the file brarr actually mapped.
//!
//! Rendered through the real router, so the assertions cover the
//! handlers, the view mapping and the templates together.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::similar_names,
    clippy::doc_markdown
)]

use std::net::SocketAddr;
use std::time::Duration;

use brarr_decision_service::Engine;
use brarr_orchestrator::db::grabs::{self, LocalGrab};
use brarr_orchestrator::db::library::{self, MediaType, NewEpisode, NewLibraryItem, NewSeason};
use brarr_orchestrator::{AppState, db, web};
use time::OffsetDateTime;
use uuid::Uuid;

async fn spawn() -> (SocketAddr, AppState) {
    let pool = db::open_memory().await.expect("open in-memory db");
    let state = AppState::new(pool, Engine::baseline());
    let static_dir = std::env::temp_dir().join("brarr-orchestrator-library-ui-static");
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
    (addr, state)
}

fn days_ago(n: i64) -> OffsetDateTime {
    OffsetDateTime::now_utc() - time::Duration::days(n)
}

fn days_ahead(n: i64) -> OffsetDateTime {
    OffsetDateTime::now_utc() + time::Duration::days(n)
}

/// One series: season 1 fully aired (3 episodes), season 2 with one aired
/// and one still to come.
async fn seed_series(state: &AppState) -> Uuid {
    let item = library::upsert(
        state.pool(),
        &NewLibraryItem {
            media_type: Some(MediaType::Tv),
            tmdb_id: 76_479,
            title: "The Boys".to_owned(),
            year: Some(2019),
            ..NewLibraryItem::default()
        },
    )
    .await
    .unwrap();
    library::sync_seasons(
        state.pool(),
        item.id,
        &[
            NewSeason {
                season_number: 1,
                episode_count: 3,
                air_date: Some(days_ago(400)),
                episodes: vec![
                    NewEpisode {
                        episode_number: 1,
                        title: Some("A".to_owned()),
                        air_date: Some(days_ago(400)),
                    },
                    NewEpisode {
                        episode_number: 2,
                        title: Some("B".to_owned()),
                        air_date: Some(days_ago(393)),
                    },
                    NewEpisode {
                        episode_number: 3,
                        title: Some("C".to_owned()),
                        air_date: Some(days_ago(386)),
                    },
                ],
            },
            NewSeason {
                season_number: 2,
                episode_count: 2,
                air_date: Some(days_ago(30)),
                episodes: vec![
                    NewEpisode {
                        episode_number: 1,
                        title: Some("D".to_owned()),
                        air_date: Some(days_ago(30)),
                    },
                    NewEpisode {
                        episode_number: 2,
                        title: Some("E".to_owned()),
                        air_date: Some(days_ahead(30)),
                    },
                ],
            },
        ],
    )
    .await
    .unwrap();
    item.id
}

/// Record a file against one episode, the way the adoption and the *arr
/// import both do: reserve, then mark imported at the same path.
async fn adopt(state: &AppState, item: Uuid, episode: Uuid, path: &str) {
    let grab = grabs::reserve_local(
        state.pool(),
        &LocalGrab {
            item_id: item,
            episode_id: Some(episode),
            source_path: path,
            release_name: path,
        },
    )
    .await
    .unwrap()
    .expect("the barrier must let the first reservation through");
    grabs::mark_imported(state.pool(), grab.id, path)
        .await
        .unwrap();
}

async fn get(addr: SocketAddr, path: &str) -> String {
    reqwest::get(format!("http://{addr}{path}"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
}

/// The operator's own question: with everything monitored and one aired
/// episode absent, the card says 4/5 and paints red — not "4 of whatever
/// the series has".
#[tokio::test]
async fn the_card_counts_monitored_content_and_flags_the_gap() {
    let (addr, state) = spawn().await;
    let item = seed_series(&state).await;
    let episodes = library::episodes(state.pool(), item).await.unwrap();
    // Everything but S02E01, which aired.
    for e in episodes
        .iter()
        .filter(|e| !(e.season_number == 2 && e.episode_number == 1))
    {
        adopt(
            &state,
            item,
            e.id,
            &format!(
                "/midias/S{:02}E{:02}.mkv",
                e.season_number, e.episode_number
            ),
        )
        .await;
    }

    let body = get(addr, "/library").await;
    assert!(body.contains("lib-status-missing"), "{body}");
    assert!(
        body.contains("lib-spine-missing"),
        "the spine must carry the same tone"
    );
    assert!(
        body.contains("4/5"),
        "denominator is the monitored episodes: {body}"
    );
    assert!(body.contains("1 faltando"), "{body}");
}

/// An episode that has not aired is not a gap. Without this split every
/// returning series is permanently red and the colour stops meaning
/// anything.
#[tokio::test]
async fn an_unaired_episode_reads_as_up_to_date_not_missing() {
    let (addr, state) = spawn().await;
    let item = seed_series(&state).await;
    let episodes = library::episodes(state.pool(), item).await.unwrap();
    for e in episodes
        .iter()
        .filter(|e| e.air_date.is_some_and(|d| d < OffsetDateTime::now_utc()))
    {
        adopt(&state, item, e.id, &format!("/midias/{}.mkv", e.id)).await;
    }

    let body = get(addr, "/library").await;
    assert!(body.contains("lib-status-current"), "{body}");
    assert!(!body.contains("lib-status-missing"));
    assert!(body.contains("1 a estrear"), "{body}");
}

/// A paused title makes no claim at all — the status must not read as a
/// judgement on content brarr was told not to chase.
#[tokio::test]
async fn a_paused_title_is_neither_complete_nor_missing() {
    let (addr, state) = spawn().await;
    let item = seed_series(&state).await;
    library::set_monitored(state.pool(), item, false)
        .await
        .unwrap();

    let body = get(addr, "/library").await;
    assert!(body.contains("lib-status-paused"), "{body}");
    assert!(!body.contains("lib-status-missing"));
    // Found on screen: the grey chip said "pausado" while red text next
    // to it said "5 faltando". Both were true, and together they read as
    // a call to action brarr was never going to take.
    assert!(
        !body.contains("faltando"),
        "a paused title must not call out a gap it will not close: {body}"
    );
    assert!(body.contains("0/5"), "the honest count still shows: {body}");
}

/// The four episode marks, and the one that carries the file name: the
/// tooltip is what turns "this episode is wrong" into "this episode is
/// tied to *that* file".
#[tokio::test]
async fn episode_rows_carry_a_state_mark_and_name_the_mapped_file() {
    let (addr, state) = spawn().await;
    let item = seed_series(&state).await;
    let episodes = library::episodes(state.pool(), item).await.unwrap();
    let s2 = library::seasons(state.pool(), item)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.season_number == 2)
        .unwrap();
    let aired_ep = episodes
        .iter()
        .find(|e| e.season_number == 2 && e.episode_number == 1)
        .unwrap();
    adopt(
        &state,
        item,
        aired_ep.id,
        "/midias/Series/The Boys/Season 02/The.Boys.S02E01.mkv",
    )
    .await;

    let body = get(addr, &format!("/library/{item}/season/{}", s2.id)).await;
    // S02E01 is here, S02E02 has not aired.
    assert!(body.contains("ep-mark-have"), "{body}");
    assert!(body.contains("ep-mark-unaired"), "{body}");
    assert!(!body.contains("ep-mark-missing"), "nothing aired is absent");
    assert!(
        body.contains("The.Boys.S02E01.mkv"),
        "the mapped file name has to be reachable from the row: {body}"
    );
    // The bookmark replaced the old monitor/pause button.
    assert!(body.contains("mark-btn"), "{body}");
    assert!(
        !body.contains(">pausar<"),
        "the text button is gone: {body}"
    );
}

/// An aired episode with nothing covering it is the case the exclamation
/// exists for.
#[tokio::test]
async fn an_aired_episode_with_no_file_is_marked_missing() {
    let (addr, state) = spawn().await;
    let item = seed_series(&state).await;
    let s1 = library::seasons(state.pool(), item)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.season_number == 1)
        .unwrap();

    let body = get(addr, &format!("/library/{item}/season/{}", s1.id)).await;
    assert_eq!(body.matches("ep-mark-missing").count(), 3, "{body}");
}

/// Toggling a season cascades to its episodes, and the response has to
/// *show* that — the operator should not have to trust it. The season's
/// own bookmark rides out-of-band because it lives outside the swap.
#[tokio::test]
async fn a_season_toggle_answers_with_every_episode_it_changed() {
    let (addr, state) = spawn().await;
    let item = seed_series(&state).await;
    let s1 = library::seasons(state.pool(), item)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.season_number == 1)
        .unwrap();

    let body = reqwest::Client::new()
        .post(format!(
            "http://{addr}/library/{item}/season/{}/monitor",
            s1.id
        ))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    // Every episode row came back, all unpressed.
    assert_eq!(body.matches("aria-pressed=\"false\"").count(), 4, "{body}");
    assert!(!body.contains("aria-pressed=\"true\""), "{body}");
    // …including the season's own bookmark, out-of-band.
    assert!(
        body.contains(&format!("id=\"season-mark-{}\"", s1.id)),
        "the season bookmark must ride along or it keeps showing the old state: {body}"
    );
    assert!(body.contains("hx-swap-oob=\"outerHTML\""), "{body}");
    // …and so does its status group. Sending only the bookmark left a
    // chip still reading "faltando" over rows that were no longer
    // monitored at all.
    assert!(
        body.contains(&format!("id=\"season-status-{}\"", s1.id)),
        "the season chip has to be refreshed too: {body}"
    );
    assert!(
        body.contains("lib-status-paused"),
        "the season that was just paused is not 'faltando' any more: {body}"
    );
    assert!(
        !body.contains("lib-status-missing"),
        "the stale chip is the bug this fragment exists to fix: {body}"
    );

    // And it actually persisted, down to the episodes.
    let episodes = library::episodes(state.pool(), item).await.unwrap();
    assert!(
        episodes
            .iter()
            .filter(|e| e.season_number == 1)
            .all(|e| !e.monitored)
    );
}

/// The acquisition history moved into a dialog. Inline, a series the
/// *arr import brought in pushed the season tree below 800 release
/// names.
#[tokio::test]
async fn the_grab_history_is_a_dialog_and_not_the_page() {
    let (addr, state) = spawn().await;
    let item = seed_series(&state).await;
    let episodes = library::episodes(state.pool(), item).await.unwrap();
    adopt(&state, item, episodes[0].id, "/midias/one.mkv").await;

    let page = get(addr, &format!("/library/{item}")).await;
    assert!(
        page.contains("grabs (1)"),
        "the count stays on the page: {page}"
    );
    assert!(
        !page.contains("/midias/one.mkv"),
        "the release list must not be inline any more: {page}"
    );

    let modal = get(addr, &format!("/library/{item}/grabs")).await;
    assert!(modal.contains("<dialog"), "{modal}");
    assert!(modal.contains("/midias/one.mkv"), "{modal}");
}

/// A movie has no episodes, so its progress is the one unit it is — and
/// an unreleased one must not read as missing.
#[tokio::test]
async fn a_movie_not_out_yet_is_upcoming_rather_than_missing() {
    let (addr, state) = spawn().await;
    library::upsert(
        state.pool(),
        &NewLibraryItem {
            media_type: Some(MediaType::Movie),
            tmdb_id: 693_134,
            title: "Duna: Parte Três".to_owned(),
            digital_release_at: Some(days_ahead(60)),
            ..NewLibraryItem::default()
        },
    )
    .await
    .unwrap();

    let body = get(addr, "/library").await;
    assert!(body.contains("lib-status-upcoming"), "{body}");
    assert!(!body.contains("lib-status-missing"), "{body}");
}
