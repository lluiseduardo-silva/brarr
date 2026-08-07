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
            // The episode search axis is TVDB, so a series without one
            // can never be swept — the fixture carries it or the sweep
            // tests would all exercise the "no axis" path instead.
            tvdb_id: Some(355_567),
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

/// A series with a specials bucket, the shape The Familiar of Zero has:
/// one special the operator monitors, plus three real episodes.
async fn seed_series_with_special(state: &AppState) -> Uuid {
    let item = library::upsert(
        state.pool(),
        &NewLibraryItem {
            media_type: Some(MediaType::Tv),
            tmdb_id: 35_753,
            title: "The Familiar of Zero".to_owned(),
            year: Some(2006),
            ..NewLibraryItem::default()
        },
    )
    .await
    .unwrap();
    let ep = |n: i32| NewEpisode {
        episode_number: n,
        title: None,
        air_date: Some(days_ago(3000)),
    };
    library::sync_seasons(
        state.pool(),
        item.id,
        &[
            NewSeason {
                season_number: 0,
                episode_count: 1,
                air_date: Some(days_ago(3000)),
                episodes: vec![ep(1)],
            },
            NewSeason {
                season_number: 1,
                episode_count: 3,
                air_date: Some(days_ago(3000)),
                episodes: vec![ep(1), ep(2), ep(3)],
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

/// A grab reserved and left in flight — no file, no import. This is what
/// makes an episode row render `ep-mark-busy`.
async fn in_flight(state: &AppState, item: Uuid, episode: Uuid, season: i32) -> Uuid {
    use brarr_orchestrator::db::grabs::{NewGrab, Protocol};
    use brarr_orchestrator::db::providers::{self, NewProvider};

    let provider = providers::insert(
        state.pool(),
        NewProvider {
            name: "capybara",
            base_url: &url::Url::parse("https://capybarabr.com/").unwrap(),
            api_token: "tok",
            kind: "unit3d",
            plugin_path: None,
        },
    )
    .await
    .unwrap();
    let grab = grabs::reserve(
        state.pool(),
        &NewGrab {
            item_id: item,
            episode_id: Some(episode),
            season_number: Some(season),
            decision_id: None,
            provider_id: provider.id,
            provider_name: "capybara",
            release_id_remote: "abc",
            release_name: "Show.S01E01.1080p.WEB-DL.PT-BR",
            download_url: None,
            protocol: Protocol::Torrent,
        },
    )
    .await
    .unwrap()
    .expect("the barrier must let the first reservation through");
    grab.id
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
    // Scoped to the season's own fragment: the response also carries the
    // *item* status, and that one legitimately still reads "faltando"
    // because season 2 is untouched. Asserting over the whole body would
    // confuse the two.
    let season_fragment = body
        .split(&format!("id=\"season-status-{}\"", s1.id))
        .nth(1)
        .and_then(|rest| rest.split("id=\"item-status\"").next())
        .unwrap_or_default();
    assert!(
        season_fragment.contains("lib-status-paused"),
        "the season that was just paused is not 'faltando' any more: {body}"
    );
    assert!(
        !season_fragment.contains("lib-status-missing"),
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

// ---------- the control row and the per-row search ----------

/// The interactive search left the header. It used to be a form with a
/// season `<select>` and an episode input sitting above the tree; the
/// search now happens where the operator is already looking.
#[tokio::test]
async fn the_interactive_search_left_the_header_for_the_rows() {
    let (addr, state) = spawn().await;
    let item = seed_series(&state).await;

    let page = get(addr, &format!("/library/{item}")).await;
    assert!(
        !page.contains("busca interativa"),
        "the header form must be gone: {page}"
    );
    assert!(
        !page.contains(r#"placeholder="ep.""#),
        "and so must its episode input: {page}"
    );
    // Season 1 gets a magnifier that searches the whole pack — no
    // `episode`, which is what the handler already reads as a pack.
    assert!(
        page.contains(&format!("/library/{item}/interactive?season=1")),
        "the season needs its own search: {page}"
    );
    // Still one target, because the results table is wide and two of
    // them on screen at once would help nobody.
    assert!(page.contains(r#"id="interactive-results""#));
}

/// Season 0 is TMDB's specials bucket and the scanner skips it
/// everywhere. The old picker omitted it; the magnifier must too, or it
/// becomes the single door into a season nothing else chases.
#[tokio::test]
async fn the_specials_season_gets_no_search_button() {
    let (addr, state) = spawn().await;
    let item = seed_series(&state).await;

    let page = get(addr, &format!("/library/{item}")).await;
    assert!(
        !page.contains(&format!("/library/{item}/interactive?season=0")),
        "season 0 must not offer a search: {page}"
    );
}

/// Each episode row carries its own magnifier, addressed by season and
/// episode number rather than by parsing `S01E02` back apart.
#[tokio::test]
async fn every_episode_row_can_search_itself() {
    let (addr, state) = spawn().await;
    let item = seed_series(&state).await;
    let seasons = library::seasons(state.pool(), item).await.unwrap();
    let first = seasons.iter().find(|s| s.season_number == 1).unwrap();

    let rows = get(addr, &format!("/library/{item}/season/{}", first.id)).await;
    assert!(
        rows.contains(&format!(
            "/library/{item}/interactive?season=1&amp;episode=1"
        )),
        "the row must search exactly its own episode: {rows}"
    );
}

/// The two `<select>`s moved behind the gear. They used to submit the
/// whole page on `change`, from inside a row of 36px squares.
#[tokio::test]
async fn placement_moved_into_a_dialog() {
    let (addr, state) = spawn().await;
    let item = seed_series(&state).await;

    let page = get(addr, &format!("/library/{item}")).await;
    assert!(
        !page.contains("this.form.submit()"),
        "no more submit-on-change in the control row: {page}"
    );
    assert!(
        page.contains(&format!("/library/{item}/placement")),
        "the gear opens the dialog: {page}"
    );

    let dialog = get(addr, &format!("/library/{item}/placement")).await;
    assert!(dialog.contains("<dialog"));
    assert!(dialog.contains(r#"name="profile_id""#));
    // Posts to the handler that already existed — this route only renders.
    assert!(dialog.contains(&format!(r#"action="/library/{item}/profile""#)));
    // Cancel is a button, never a nested `<form method="dialog">`: the
    // parser drops the inner tag and it becomes a submit of the outer.
    assert!(
        !dialog.contains(r#"method="dialog""#),
        "cancel must not be a nested dialog form: {dialog}"
    );
}

// ---------- item 4: the sweep, pointed at one target ----------

/// The lightning bolt is the automatic sweep, addressed at one episode.
/// It sits beside the magnifier rather than replacing it: one shows what
/// exists and lets the operator choose, the other decides.
#[tokio::test]
async fn every_episode_row_can_run_the_sweep_on_itself() {
    let (addr, state) = spawn().await;
    let item = seed_series(&state).await;
    let seasons = library::seasons(state.pool(), item).await.unwrap();
    let first = seasons.iter().find(|s| s.season_number == 1).unwrap();

    let rows = get(addr, &format!("/library/{item}/season/{}", first.id)).await;
    assert!(
        rows.contains(&format!(
            "/library/{item}/scan/target?season=1&amp;episode=1"
        )),
        "the row must sweep exactly its own episode: {rows}"
    );
    // Both buttons, both slots — the two are different actions.
    assert!(rows.contains(&format!(
        "/library/{item}/interactive?season=1&amp;episode=1"
    )));
    assert!(rows.contains(&format!(r#"id="scan-ep-{item}-1-1""#)));
}

/// A narrowed sweep respects monitoring — pausing is the operator's
/// standing decision and a one-click shortcut must not overrule it. What
/// it must not do is fail silently: the badge has to say *paused*, not
/// "nada encontrado", because nothing was even searched.
#[tokio::test]
async fn sweeping_a_paused_episode_says_paused_rather_than_nothing_found() {
    let (addr, state) = spawn().await;
    let item = seed_series(&state).await;
    let episodes = library::episodes(state.pool(), item).await.unwrap();
    let ep = episodes
        .iter()
        .find(|e| e.season_number == 1 && e.episode_number == 1)
        .unwrap();
    library::set_episode_monitored(state.pool(), ep.id, false)
        .await
        .unwrap();

    let body = reqwest::Client::new()
        .post(format!(
            "http://{addr}/library/{item}/scan/target?season=1&episode=1"
        ))
        .send()
        .await
        .expect("send")
        .text()
        .await
        .unwrap();
    assert!(body.contains("pausado"), "got: {body}");
    assert!(
        !body.contains("nada encontrado"),
        "nothing was searched, so that label would be a lie: {body}"
    );
    // And it points at the escape hatch.
    assert!(body.contains("lupa"), "got: {body}");
}

/// A movie has no seasons, so a narrowed sweep of one is a
/// contradiction. Refuse it rather than quietly sweeping the whole film.
#[tokio::test]
async fn a_narrowed_sweep_of_a_movie_finds_no_targets() {
    let (addr, state) = spawn().await;
    let item = library::upsert(
        state.pool(),
        &library::NewLibraryItem {
            media_type: Some(MediaType::Movie),
            tmdb_id: 603,
            title: "The Matrix".to_owned(),
            ..library::NewLibraryItem::default()
        },
    )
    .await
    .unwrap();

    let resp = reqwest::Client::new()
        .post(format!(
            "http://{addr}/library/{}/scan/target?season=1",
            item.id
        ))
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        !body.contains("grab(s)"),
        "a movie must not be swept by a season button: {body}"
    );
}

// ---------- item 2: the number on the row ----------

/// The percentage comes from the cache the queue sync fills, so the row
/// costs no HTTP call. With nothing cached the row still renders — the
/// busy icon without a number, which is what it did before.
#[tokio::test]
async fn an_episode_row_shows_the_cached_download_percentage() {
    use std::time::Instant;

    let (addr, state) = spawn().await;
    let item = seed_series(&state).await;
    let episodes = library::episodes(state.pool(), item).await.unwrap();
    let ep = episodes
        .iter()
        .find(|e| e.season_number == 1 && e.episode_number == 1)
        .unwrap();
    let seasons = library::seasons(state.pool(), item).await.unwrap();
    let first = seasons.iter().find(|s| s.season_number == 1).unwrap();

    let grab_id = in_flight(&state, item, ep.id, ep.season_number).await;

    // Before the sync has seen it: the icon, no number.
    let before = get(addr, &format!("/library/{item}/season/{}", first.id)).await;
    assert!(before.contains("ep-mark-busy"), "{before}");
    assert!(
        !before.contains("lib-bar-fill\" style=\"width:42%"),
        "nothing cached yet, so no number: {before}"
    );

    state.progress().insert(grab_id, 42, Instant::now());

    let after = get(addr, &format!("/library/{item}/season/{}", first.id)).await;
    assert!(
        after.contains("42%"),
        "the number must reach the row: {after}"
    );
    assert!(
        after.contains(r#"style="width:42%""#),
        "and so must the bar: {after}"
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
    // The button became an icon, so the count moved into the corner
    // badge and the tooltip. Losing the number was the one thing that
    // would have made the icon row a downgrade — it is what makes the
    // operator click.
    assert!(
        page.contains(r#"<span class="btn-count">1</span>"#),
        "the count stays on the page: {page}"
    );
    assert!(
        page.contains("Histórico de aquisições deste título (1)"),
        "and it is in the tooltip too: {page}"
    );
    assert!(
        !page.contains("/midias/one.mkv"),
        "the release list must not be inline any more: {page}"
    );

    let modal = get(addr, &format!("/library/{item}/grabs")).await;
    assert!(modal.contains("<dialog"), "{modal}");
    assert!(modal.contains("/midias/one.mkv"), "{modal}");
}

/// Reported from the screen: The Familiar of Zero has one monitored
/// special, on disk, and the card read 49/49 instead of 50/50.
///
/// The count follows monitoring and nothing else — and the second half
/// matters as much: unmarking the specials season has to *change* the
/// number, or the operator's click is a no-op.
#[tokio::test]
async fn a_monitored_special_counts_and_unmarking_it_changes_the_number() {
    let (addr, state) = spawn().await;
    let item = seed_series_with_special(&state).await;
    let episodes = library::episodes(state.pool(), item).await.unwrap();
    for e in &episodes {
        adopt(&state, item, e.id, &format!("/midias/{}.mkv", e.id)).await;
    }

    // 3 real episodes + 1 special, all monitored and all on disk.
    let body = get(addr, &format!("/library/{item}")).await;
    assert!(body.contains("4/4"), "the special is one of them: {body}");
    assert!(body.contains("lib-status-complete"), "{body}");

    // Unmark the specials season: the count has to drop to 3/3.
    let specials = library::seasons(state.pool(), item)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.season_number == 0)
        .unwrap();
    let after = reqwest::Client::new()
        .post(format!(
            "http://{addr}/library/{item}/season/{}/monitor",
            specials.id
        ))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    // The hero rides back out-of-band — leaving it stale is what made
    // unmarking a season look like it did nothing.
    assert!(
        after.contains("id=\"item-status\""),
        "the hero has to be re-sent: {after}"
    );
    assert!(after.contains("3/3"), "{after}");
    assert!(get(addr, &format!("/library/{item}")).await.contains("3/3"));
}

/// One monitored episode toggle also moves the denominator, so the hero
/// has to come back from that one too.
#[tokio::test]
async fn an_episode_toggle_refreshes_the_item_status() {
    let (addr, state) = spawn().await;
    let item = seed_series(&state).await;
    let episodes = library::episodes(state.pool(), item).await.unwrap();

    let body = reqwest::Client::new()
        .post(format!(
            "http://{addr}/library/{item}/episode/{}/monitor",
            episodes[0].id
        ))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("id=\"item-status\""), "{body}");
    assert!(body.contains("hx-swap-oob"), "{body}");
    // Five monitored episodes became four.
    assert!(body.contains("0/4"), "{body}");
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
