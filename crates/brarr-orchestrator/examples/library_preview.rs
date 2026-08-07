//! A seeded library, served on :3999, for looking at the status surface.
//!
//! The integration tests assert the markup; this exists to check that the
//! CSS actually applies — a rule can be present and still be overridden.
//! In-memory database, no TMDB, no network: nothing here can reach the
//! operator's stack.
//!
//! ```text
//! cargo run -p brarr-orchestrator --example library_preview
//! ```

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    reason = "developer-facing preview harness"
)]

use brarr_decision_service::Engine;
use brarr_orchestrator::db::grabs::{self, LocalGrab};
use brarr_orchestrator::db::library::{self, MediaType, NewEpisode, NewLibraryItem, NewSeason};
use brarr_orchestrator::{AppState, db, web};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

fn ago(n: i64) -> OffsetDateTime {
    OffsetDateTime::now_utc() - Duration::days(n)
}
fn ahead(n: i64) -> OffsetDateTime {
    OffsetDateTime::now_utc() + Duration::days(n)
}

async fn series(state: &AppState, tmdb: i64, title: &str, seasons: &[NewSeason]) -> Uuid {
    let item = library::upsert(
        state.pool(),
        &NewLibraryItem {
            media_type: Some(MediaType::Tv),
            tmdb_id: tmdb,
            title: title.to_owned(),
            year: Some(2019),
            ..NewLibraryItem::default()
        },
    )
    .await
    .unwrap();
    library::sync_seasons(state.pool(), item.id, seasons)
        .await
        .unwrap();
    item.id
}

fn season(number: i32, aired: usize, upcoming: usize) -> NewSeason {
    let mut episodes = Vec::new();
    for i in 0..aired {
        episodes.push(NewEpisode {
            episode_number: i32::try_from(i).unwrap_or(0) + 1,
            title: Some(format!("Episódio {}", i + 1)),
            air_date: Some(ago(300 - i64::try_from(i).unwrap_or(0) * 7)),
        });
    }
    for i in 0..upcoming {
        episodes.push(NewEpisode {
            episode_number: i32::try_from(aired + i).unwrap_or(0) + 1,
            title: Some(format!("Episódio {}", aired + i + 1)),
            air_date: Some(ahead(7 + i64::try_from(i).unwrap_or(0) * 7)),
        });
    }
    NewSeason {
        season_number: number,
        episode_count: i32::try_from(episodes.len()).unwrap_or(0),
        air_date: Some(ago(300)),
        episodes,
    }
}

async fn adopt(state: &AppState, item: Uuid, episode: Uuid, path: &str) {
    if let Some(grab) = grabs::reserve_local(
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
    {
        grabs::mark_imported(state.pool(), grab.id, path)
            .await
            .unwrap();
    }
}

/// Give an item files for the first `n` of its episodes.
async fn fill(state: &AppState, item: Uuid, n: usize) {
    for e in library::episodes(state.pool(), item)
        .await
        .unwrap()
        .iter()
        .take(n)
    {
        adopt(
            state,
            item,
            e.id,
            &format!(
                "/midias/Series/Exemplo/Season {:02}/Exemplo.S{:02}E{:02}.1080p.WEB-DL.mkv",
                e.season_number, e.season_number, e.episode_number
            ),
        )
        .await;
    }
}

#[tokio::main]
async fn main() {
    let pool = db::open_memory().await.unwrap();
    let state = AppState::new(pool, Engine::baseline());

    // One title per status, so the whole colour vocabulary is on screen
    // at once and they can be compared side by side.
    let complete = series(&state, 1, "Completa (verde)", &[season(1, 4, 0)]).await;
    fill(&state, complete, 4).await;

    let current = series(&state, 2, "Em dia (azul)", &[season(1, 3, 2)]).await;
    fill(&state, current, 3).await;

    let missing = series(
        &state,
        3,
        "Faltando (vermelho)",
        &[season(1, 5, 0), season(2, 4, 1)],
    )
    .await;
    fill(&state, missing, 6).await;

    series(&state, 4, "A estrear (roxo)", &[season(1, 0, 6)]).await;

    let paused = series(&state, 5, "Pausada (cinza)", &[season(1, 4, 0)]).await;
    library::set_monitored(state.pool(), paused, false)
        .await
        .unwrap();

    library::upsert(
        state.pool(),
        &NewLibraryItem {
            media_type: Some(MediaType::Movie),
            tmdb_id: 6,
            title: "Filme sem arquivo".to_owned(),
            year: Some(2021),
            ..NewLibraryItem::default()
        },
    )
    .await
    .unwrap();

    let static_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("static");
    let router = web::router(state, &static_dir);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3999")
        .await
        .unwrap();
    println!("library preview → http://127.0.0.1:3999/library");
    println!("  detalhe da série 'Faltando' → http://127.0.0.1:3999/library/{missing}");
    axum::serve(listener, router).await.unwrap();
}
