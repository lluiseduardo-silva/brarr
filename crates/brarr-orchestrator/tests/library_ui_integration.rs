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
            // Needed by the sweep tests: without it `build_targets`
            // bails on "no search axis" before it looks at a season.
            tvdb_id: Some(79_183),
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
    // The *callout* specifically — the word also lives in the
    // "Episódios faltando" filter chip, which is navigation and not a
    // claim about any title.
    assert!(
        !body.contains(" faltando</span>"),
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
    // them on screen at once would help nobody — it is the house modal
    // slot now, not a loose div under the hero.
    assert!(page.contains(r##"hx-target="#modal-target""##));
}

/// The results open in a dialog the operator can actually dismiss.
///
/// They used to land in a loose `<div>` under the hero, and once filled
/// there was no way to close it — the only exit was navigating to
/// another screen and back.
#[tokio::test]
async fn the_interactive_results_open_in_a_dismissable_dialog() {
    let (addr, state) = spawn().await;
    let item = seed_series(&state).await;

    let page = get(addr, &format!("/library/{item}")).await;
    assert!(
        !page.contains(r#"id="interactive-results""#),
        "the loose div must be gone: {page}"
    );
    // `r##` because the literal itself contains `"#`, which would close
    // a single-hash raw string.
    assert!(
        page.contains(r##"hx-target="#modal-target""##),
        "the magnifier must open the house modal slot: {page}"
    );

    let results = get(addr, &format!("/library/{item}/interactive?season=1")).await;
    assert!(results.contains("<dialog"), "got: {results}");
    assert!(results.contains(r#"id="interactive-dialog""#));
    // Two ways out, and neither is a nested `<form method="dialog">` —
    // the parser drops that and it becomes a submit of the outer form.
    assert_eq!(
        results.matches("interactive-dialog').close()").count(),
        2,
        "an X in the header and a Fechar in the footer: {results}"
    );
    assert!(!results.contains(r#"method="dialog""#));
}

/// Season 0 gets the same buttons as any other season.
///
/// It used to get none, because the scanner excluded the specials
/// bucket. Now that the sweep honours the monitoring flag there too,
/// omitting them would recreate the same confusion one size smaller:
/// counted in the tree, swept by the item button, and no action on its
/// own row.
#[tokio::test]
async fn the_specials_season_gets_the_same_buttons_as_any_other() {
    let (addr, state) = spawn().await;
    let item = seed_series_with_special(&state).await;

    let page = get(addr, &format!("/library/{item}")).await;
    assert!(
        page.contains(&format!("/library/{item}/interactive?season=0")),
        "season 0 needs its magnifier: {page}"
    );
    assert!(
        page.contains(&format!("/library/{item}/scan/target?season=0")),
        "and its sweep: {page}"
    );
}

/// The asymmetry that closed: `coverage` counts a monitored special, so
/// the sweep has to be able to go after it. A sweep of season 0 that
/// found no targets would mean the tree flags a gap nothing will ever
/// close.
#[tokio::test]
async fn sweeping_the_specials_season_actually_builds_a_target() {
    let (addr, state) = spawn().await;
    let item = seed_series_with_special(&state).await;
    let episodes = library::episodes(state.pool(), item).await.unwrap();
    let special = episodes
        .iter()
        .find(|e| e.season_number == 0)
        .expect("the fixture has a specials bucket");
    library::set_episode_monitored(state.pool(), special.id, true)
        .await
        .unwrap();

    let body = reqwest::Client::new()
        .post(format!("http://{addr}/library/{item}/scan/target?season=0"))
        .send()
        .await
        .expect("send")
        .text()
        .await
        .unwrap();
    // No providers configured, so the honest verdict is "searched and
    // found nothing" — not "paused" and not "no targets".
    assert!(
        body.contains("nada encontrado"),
        "a monitored special must be searched, got: {body}"
    );
    assert!(!body.contains("pausado"), "got: {body}");
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

/// The stale-stylesheet bug, closed from both ends.
///
/// `ServeDir` sends only `Last-Modified`, and a response with no
/// explicit directive is *heuristically* cacheable — the browser invents
/// an expiry. That is how fresh markup ended up rendered against a
/// days-old `app.css`, with every new class silently inert.
#[tokio::test]
async fn static_assets_are_versioned_and_must_be_revalidated() {
    let (addr, _state) = spawn().await;

    let page = get(addr, "/library").await;
    let stamp = format!("?v={}", brarr_orchestrator::web::ASSET_VERSION);
    assert!(
        page.contains(&format!("/static/app.css{stamp}")),
        "the stylesheet needs a URL that changes with the release: {page}"
    );
    assert!(
        page.contains(&format!("/static/library.js{stamp}")),
        "and so does every script: {page}"
    );

    let resp = reqwest::get(format!("http://{addr}/static/app.css"))
        .await
        .expect("send");
    assert_eq!(
        resp.headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("no-cache"),
        "without a directive the browser is free to invent an expiry"
    );
}

// ---------- the catalogue: filters, search, bulk ----------

/// Selection is a mode, and a chosen title says so with its border.
///
/// 360 native checkboxes are noise on a grid whose job is being read at
/// a glance, and a bare `input[type=checkbox]` has no identity. The box
/// stays in the DOM — it is what the form submits — and is never drawn.
#[tokio::test]
async fn selection_is_a_mode_and_the_box_is_never_drawn() {
    let (addr, state) = spawn().await;
    seed_series(&state).await;

    let page = get(addr, "/library").await;
    assert!(
        page.contains("data-select-toggle"),
        "there has to be a way into the mode: {page}"
    );
    assert!(page.contains("data-select-off"), "and a way out: {page}");
    // The card is the click target and carries the selected styling.
    assert!(page.contains("lib-pick card-hairline"), "{page}");
    assert!(page.contains(r#"class="lib-pick-box""#), "{page}");
    // The old visible checkbox is gone from the rows; `.lib-check` now
    // dresses only the "select all" control in the bar.
    assert_eq!(
        page.matches("lib-check").count(),
        1,
        "only the master box is drawn: {page}"
    );
    // Screen readers still get a name for it.
    assert!(
        page.contains("aria-label=\"Selecionar The Boys\""),
        "{page}"
    );
}

/// The two new chips read a status `coverage` computes, not a column.
///
/// "Faltando" is deliberately what the operator can *act on*: monitored,
/// already aired, and absent. A paused title with the same gaps stays
/// out — brarr is not going to chase it, so listing it would be a call
/// to an action that does not exist.
#[tokio::test]
async fn the_missing_and_complete_chips_filter_by_status() {
    let (addr, state) = spawn().await;
    let gap = seed_series(&state).await;

    // A second series with every aired episode on disk.
    let done = seed_series_with_special(&state).await;
    for ep in library::episodes(state.pool(), done).await.unwrap() {
        adopt(&state, done, ep.id, &format!("/midias/{}.mkv", ep.id)).await;
    }

    let missing = get(addr, "/library?filter=missing").await;
    assert!(missing.contains("The Boys"), "{missing}");
    assert!(!missing.contains("The Familiar of Zero"), "{missing}");

    let complete = get(addr, "/library?filter=complete").await;
    assert!(complete.contains("The Familiar of Zero"), "{complete}");
    assert!(!complete.contains("The Boys"), "{complete}");

    // Pausing the gap-ridden one takes it out of "faltando": brarr will
    // not go after it, so it is not a gap the operator can close.
    library::set_monitored(state.pool(), gap, false)
        .await
        .unwrap();
    let after = get(addr, "/library?filter=missing").await;
    assert!(!after.contains("The Boys"), "{after}");
}

/// The case the operator named: the Japanese name of an anime finds the
/// localised title, because `original_title` is a field we already
/// store. No edit distance would ever connect the two.
#[tokio::test]
async fn searching_the_original_title_finds_the_localised_one() {
    let (addr, state) = spawn().await;
    library::upsert(
        state.pool(),
        &NewLibraryItem {
            media_type: Some(MediaType::Tv),
            tmdb_id: 65_942,
            title: "That Time I Got Reincarnated as a Slime".to_owned(),
            original_title: Some("Tensei Shitara Slime Datta Ken".to_owned()),
            ..NewLibraryItem::default()
        },
    )
    .await
    .unwrap();

    let found = get(addr, "/library?q=tensei+shitara").await;
    assert!(found.contains("Reincarnated"), "{found}");

    // And the tolerances the box advertises.
    assert!(
        get(addr, "/library?q=reincarnted")
            .await
            .contains("Reincarnated")
    );
    assert!(
        get(addr, "/library?q=slime+tensei")
            .await
            .contains("Reincarnated")
    );

    let nothing = get(addr, "/library?q=breaking+bad").await;
    assert!(nothing.contains("Nada encontrado"), "{nothing}");
}

/// Accents are optional when typing and mandatory in the data.
#[tokio::test]
async fn searching_ignores_accents_and_punctuation() {
    let (addr, state) = spawn().await;
    library::upsert(
        state.pool(),
        &NewLibraryItem {
            media_type: Some(MediaType::Tv),
            tmdb_id: 94_997,
            title: "A Casa do Dragão".to_owned(),
            ..NewLibraryItem::default()
        },
    )
    .await
    .unwrap();

    assert!(
        get(addr, "/library?q=casa+do+dragao")
            .await
            .contains("Dragão")
    );
    assert!(get(addr, "/library?q=DRAGÃO").await.contains("Dragão"));
}

/// The fragment route is the same code path as the page, so a live
/// search and a reload can never disagree about what matches.
#[tokio::test]
async fn the_items_fragment_is_the_list_without_the_page() {
    let (addr, state) = spawn().await;
    seed_series(&state).await;

    let fragment = get(addr, "/library/items?q=boys").await;
    assert!(fragment.contains("The Boys"), "{fragment}");
    assert!(
        !fragment.contains("<!DOCTYPE"),
        "a fragment must not carry the layout: {fragment}"
    );
    assert!(!fragment.contains("<nav"));
    assert!(fragment.contains(r#"id="library-body""#));
}

/// One action, every checked title. The ids ride in the form the way
/// the import screen does it — the DOM is the selection store.
#[tokio::test]
async fn a_bulk_action_moves_every_selected_title() {
    let (addr, state) = spawn().await;
    let a = seed_series(&state).await;
    let b = seed_series_with_special(&state).await;
    assert!(library::get_by_id(state.pool(), a).await.unwrap().monitored);

    let body = reqwest::Client::new()
        .post(format!("http://{addr}/library/bulk"))
        .form(&[
            ("action", "unmonitor"),
            ("sel", &a.to_string()),
            ("sel", &b.to_string()),
        ])
        .send()
        .await
        .expect("send")
        .text()
        .await
        .unwrap();
    // Answers the re-rendered list, not a redirect, so the operator does
    // not lose the filter they were in.
    assert!(body.contains(r#"id="library-body""#), "{body}");

    assert!(!library::get_by_id(state.pool(), a).await.unwrap().monitored);
    assert!(!library::get_by_id(state.pool(), b).await.unwrap().monitored);
}

/// A page can be minutes old. A title deleted in another tab must not
/// abort the other updates.
#[tokio::test]
async fn a_bulk_action_skips_ids_that_no_longer_exist() {
    let (addr, state) = spawn().await;
    let real = seed_series(&state).await;
    let ghost = Uuid::new_v4().to_string();

    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/library/bulk"))
        .form(&[
            ("action", "unmonitor"),
            ("sel", &real.to_string()),
            ("sel", &ghost),
        ])
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    assert!(
        !library::get_by_id(state.pool(), real)
            .await
            .unwrap()
            .monitored
    );
}

/// Four orderings, and the default depends on whether there is a query.
#[tokio::test]
async fn the_catalogue_can_be_ordered_by_name_and_by_date() {
    let (addr, state) = spawn().await;
    // Added oldest-first so "recently added" and "A → Z" disagree.
    for (tmdb, title) in [(1, "Zulu"), (2, "Ávatar"), (3, "melancolia")] {
        library::upsert(
            state.pool(),
            &NewLibraryItem {
                media_type: Some(MediaType::Movie),
                tmdb_id: tmdb,
                title: title.to_owned(),
                ..NewLibraryItem::default()
            },
        )
        .await
        .unwrap();
    }

    let order = |body: &str| -> Vec<String> {
        ["Zulu", "Ávatar", "melancolia"]
            .into_iter()
            .filter_map(|t| body.find(t).map(|at| (at, t.to_owned())))
            .collect::<std::collections::BTreeMap<_, _>>()
            .into_values()
            .collect()
    };

    // Alphabetical folds accents and case: byte order would put every
    // capital before every lowercase and file "Ávatar" after "Zulu".
    let asc = order(&get(addr, "/library?sort=title_asc").await);
    assert_eq!(asc, vec!["Ávatar", "melancolia", "Zulu"], "{asc:?}");

    let desc = order(&get(addr, "/library?sort=title_desc").await);
    assert_eq!(desc, vec!["Zulu", "melancolia", "Ávatar"], "{desc:?}");

    // The three land within the same instant, so their `added_at` can
    // tie and the order among ties is arbitrary. The invariant that
    // holds regardless is that the two directions are each other's
    // reverse — which is what "crescente e decrescente" means.
    let oldest = order(&get(addr, "/library?sort=added_asc").await);
    let newest = order(&get(addr, "/library?sort=added_desc").await);
    assert_eq!(oldest.len(), 3);
    assert_eq!(
        oldest.iter().rev().cloned().collect::<Vec<_>>(),
        newest,
        "asc must be desc backwards: {oldest:?} vs {newest:?}"
    );
}

/// A value the operator can mistype in the URL must reorder nothing,
/// not answer 400.
#[tokio::test]
async fn an_unknown_sort_falls_back_to_the_default() {
    let (addr, state) = spawn().await;
    seed_series(&state).await;

    let resp = reqwest::get(format!("http://{addr}/library?sort=banana"))
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("The Boys"), "{body}");
    // And the picker shows the default as chosen, not the bad value.
    assert!(body.contains(r#"<option value="" selected>"#), "{body}");
}

/// Choosing an order must survive a filter change and a search, or the
/// two controls fight each other.
#[tokio::test]
async fn the_ordering_travels_with_the_filter_and_the_search() {
    let (addr, state) = spawn().await;
    seed_series(&state).await;

    let page = get(addr, "/library?sort=title_asc").await;
    // Six chips plus two view links, each carrying the choice — a chip
    // that dropped it would silently reset the order on every click.
    assert!(
        page.matches("sort=title_asc").count() >= 8,
        "every navigation link has to carry it: {page}"
    );
    assert!(page.contains("filter=tv"), "{page}");
    // The search form sends it too, so typing does not undo the choice.
    assert!(
        page.contains(r#"<input type="hidden" name="sort" value="title_asc">"#),
        "{page}"
    );
}

/// The bulk picker offers **every** root, not one media type's.
///
/// It was hard-coded to `Tv`, so an operator with `/midias/Filmes`,
/// `/midias/Animes` and `/midias/Series` saw only the last two and the
/// movie root was simply unreachable from here.
#[tokio::test]
async fn the_bulk_root_picker_offers_movie_folders_too() {
    use brarr_orchestrator::db::root_folders;

    let (addr, state) = spawn().await;
    seed_series(&state).await;
    let movies = std::env::temp_dir().join("brarr-bulk-root-movies");
    let series = std::env::temp_dir().join("brarr-bulk-root-series");
    tokio::fs::create_dir_all(&movies).await.unwrap();
    tokio::fs::create_dir_all(&series).await.unwrap();
    root_folders::insert(
        state.pool(),
        &movies.to_string_lossy(),
        Some(MediaType::Movie),
    )
    .await
    .unwrap();
    root_folders::insert(state.pool(), &series.to_string_lossy(), Some(MediaType::Tv))
        .await
        .unwrap();

    let page = get(addr, "/library").await;
    assert!(
        page.contains(&format!("{} (filmes)", movies.to_string_lossy())),
        "the movie root has to be reachable from the bulk bar: {page}"
    );
    assert!(page.contains(&format!("{} (séries)", series.to_string_lossy())));
}

/// Offering every root is only safe because applying one is checked. A
/// movies-only root written onto a series would put a season tree inside
/// the film library, and a mixed selection is the normal case here.
#[tokio::test]
async fn a_root_that_does_not_serve_the_type_is_skipped_and_reported() {
    use brarr_orchestrator::db::root_folders;

    let (addr, state) = spawn().await;
    let series = seed_series(&state).await;
    let movie = library::upsert(
        state.pool(),
        &NewLibraryItem {
            media_type: Some(MediaType::Movie),
            tmdb_id: 603,
            title: "The Matrix".to_owned(),
            ..NewLibraryItem::default()
        },
    )
    .await
    .unwrap();
    let movies = std::env::temp_dir().join("brarr-skip-root-movies");
    tokio::fs::create_dir_all(&movies).await.unwrap();
    root_folders::insert(
        state.pool(),
        &movies.to_string_lossy(),
        Some(MediaType::Movie),
    )
    .await
    .unwrap();

    let body = reqwest::Client::new()
        .post(format!("http://{addr}/library/bulk"))
        .form(&[
            ("action", "root"),
            ("sel", &series.to_string()),
            ("sel", &movie.id.to_string()),
            ("root_folder", &movies.to_string_lossy()),
        ])
        .send()
        .await
        .expect("send")
        .text()
        .await
        .unwrap();

    let after_movie = library::get_by_id(state.pool(), movie.id).await.unwrap();
    let after_series = library::get_by_id(state.pool(), series).await.unwrap();
    assert_eq!(
        after_movie.root_folder.as_deref(),
        Some(movies.to_string_lossy().as_ref()),
        "the movie is what this root serves"
    );
    assert_eq!(
        after_series.root_folder, None,
        "the series must not be pointed at the film library"
    );
    // And it says so, rather than skipping in silence.
    assert!(body.contains("ficaram de fora"), "{body}");
}

/// A root that serves either type refuses nothing.
#[tokio::test]
async fn a_root_serving_any_type_is_applied_to_everything() {
    use brarr_orchestrator::db::root_folders;

    let (addr, state) = spawn().await;
    let series = seed_series(&state).await;
    let any = std::env::temp_dir().join("brarr-any-root");
    tokio::fs::create_dir_all(&any).await.unwrap();
    root_folders::insert(state.pool(), &any.to_string_lossy(), None)
        .await
        .unwrap();

    let body = reqwest::Client::new()
        .post(format!("http://{addr}/library/bulk"))
        .form(&[
            ("action", "root"),
            ("sel", &series.to_string()),
            ("root_folder", &any.to_string_lossy()),
        ])
        .send()
        .await
        .expect("send")
        .text()
        .await
        .unwrap();

    let after = library::get_by_id(state.pool(), series).await.unwrap();
    assert_eq!(
        after.root_folder.as_deref(),
        Some(any.to_string_lossy().as_ref())
    );
    assert!(
        !body.contains("ficaram de fora"),
        "nothing was skipped: {body}"
    );
}

/// Nothing checked is not an error, and must not touch anything.
#[tokio::test]
async fn a_bulk_action_with_an_empty_selection_is_a_no_op() {
    let (addr, state) = spawn().await;
    let item = seed_series(&state).await;

    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/library/bulk"))
        .form(&[("action", "unmonitor")])
        .send()
        .await
        .expect("send");
    assert_eq!(resp.status(), 200);
    assert!(
        library::get_by_id(state.pool(), item)
            .await
            .unwrap()
            .monitored,
        "an empty selection must leave the catalogue alone"
    );
}

/// Setting a folder in bulk must not blank the profiles. `set_placement`
/// writes both columns; the bulk setters deliberately write one each.
#[tokio::test]
async fn a_bulk_root_folder_leaves_the_profile_alone() {
    let (addr, state) = spawn().await;
    let item = seed_series(&state).await;
    let profile = brarr_orchestrator::db::quality_profiles::list_all(state.pool())
        .await
        .unwrap()
        .into_iter()
        .next()
        .expect("the presets are seeded by the migration");
    library::set_placement(state.pool(), item, Some(profile.id), None)
        .await
        .unwrap();

    reqwest::Client::new()
        .post(format!("http://{addr}/library/bulk"))
        .form(&[
            ("action", "root"),
            ("sel", &item.to_string()),
            ("root_folder", "/midias/Series"),
        ])
        .send()
        .await
        .expect("send");

    let after = library::get_by_id(state.pool(), item).await.unwrap();
    assert_eq!(after.root_folder.as_deref(), Some("/midias/Series"));
    assert_eq!(
        after.profile_id,
        Some(profile.id),
        "the folder action must not blank a hand-picked profile"
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

/// The rows re-request themselves while something is downloading, and
/// stop when nothing is. `/queue` polls the same way, and the operator
/// noticed the detail screen lagging behind it.
#[tokio::test]
async fn the_episode_rows_poll_only_while_something_is_downloading() {
    let (addr, state) = spawn().await;
    let item = seed_series(&state).await;
    let episodes = library::episodes(state.pool(), item).await.unwrap();
    let ep = episodes
        .iter()
        .find(|e| e.season_number == 1 && e.episode_number == 1)
        .unwrap();
    let seasons = library::seasons(state.pool(), item).await.unwrap();
    let first = seasons.iter().find(|s| s.season_number == 1).unwrap();
    let url = format!("/library/{item}/season/{}", first.id);

    // Nothing in flight: a plain wrapper, no trigger. Polling a static
    // list of episodes would be pure noise.
    let idle = get(addr, &url).await;
    assert!(idle.contains(&format!(r#"id="season-rows-{}""#, first.id)));
    assert!(
        !idle.contains("hx-trigger"),
        "a settled season must not ask again: {idle}"
    );

    in_flight(&state, item, ep.id, ep.season_number).await;

    let busy = get(addr, &url).await;
    let active = brarr_orchestrator::queue::LIVE_POLL_ACTIVE.as_secs();
    assert!(
        busy.contains(&format!(r#"hx-trigger="every {active}s""#)),
        "a downloading episode must keep the rows fresh: {busy}"
    );
    assert!(busy.contains(&format!("hx-get=\"/library/{item}/season/{}\"", first.id)));
}

/// A single-row response must not carry the wrapper: it is swapped
/// straight into `#ep-{id}`, so wrapping would nest a second
/// `season-rows-…` inside the list and duplicate its id.
#[tokio::test]
async fn an_episode_toggle_answers_with_the_row_and_no_wrapper() {
    let (addr, state) = spawn().await;
    let item = seed_series(&state).await;
    let episodes = library::episodes(state.pool(), item).await.unwrap();
    let ep = &episodes[0];

    let body = reqwest::Client::new()
        .post(format!(
            "http://{addr}/library/{item}/episode/{}/monitor",
            ep.id
        ))
        .send()
        .await
        .expect("send")
        .text()
        .await
        .unwrap();
    assert!(body.contains(&format!(r#"id="ep-{}""#, ep.id)));
    assert!(
        !body.contains("season-rows-"),
        "one row is not a season body: {body}"
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

/// The orderings panel renders what TMDB answered, and says plainly
/// when nothing renumbers — which is the common answer and the one an
/// operator most needs stated rather than implied by an empty table.
#[test]
fn the_orderings_panel_separates_alternates_from_the_canonical_order() {
    use askama::Template;
    use brarr_orchestrator::web::templates::{GroupRow, LibraryGroupsModalPartial};

    let canonical_only = LibraryGroupsModalPartial {
        item_title: "The Boys".to_owned(),
        rows: vec![GroupRow {
            name: "Original Air Date".to_owned(),
            kind: "exibição original".to_owned(),
            alternate: false,
            group_count: 4,
            episode_count: 32,
        }],
        alternates: 0,
    };
    let html = canonical_only.render().unwrap();
    assert!(html.contains("Original Air Date"));
    assert!(
        html.contains("Nenhuma delas renumera"),
        "a table of one canonical ordering must not read as an option"
    );

    let with_absolute = LibraryGroupsModalPartial {
        item_title: "Dragon Ball Super".to_owned(),
        rows: vec![GroupRow {
            name: "Absolute Order".to_owned(),
            kind: "absoluta".to_owned(),
            alternate: true,
            group_count: 1,
            episode_count: 131,
        }],
        alternates: 1,
    };
    let html = with_absolute.render().unwrap();
    assert!(html.contains("Absolute Order"));
    assert!(html.contains("131"));
    assert!(
        html.contains("falta é o casador"),
        "the panel has to say brarr cannot search these yet, or it promises a feature"
    );

    let none = LibraryGroupsModalPartial {
        item_title: "Um filme qualquer".to_owned(),
        rows: Vec::new(),
        alternates: 0,
    };
    assert!(none.render().unwrap().contains("não registra nenhuma"));
}
