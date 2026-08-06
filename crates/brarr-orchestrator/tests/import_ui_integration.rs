//! Integration tests for the import-from-disk dialog, driven through the
//! real router.
//!
//! The point of testing this over HTTP rather than by calling
//! `adopt::plan` directly is that the dialog's contract *is* the form:
//! every row round-trips its target and its fingerprint through hidden
//! fields, and the confirm handler rebuilds the plan and matches on
//! them. A unit test of the planner cannot catch a template that stops
//! emitting `fp`, or a handler that reads the wrong field name — and
//! either one silently imports nothing.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::similar_names,
    clippy::doc_markdown
)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use brarr_decision_service::Engine;
use brarr_orchestrator::db::library::{MediaType, NewEpisode, NewLibraryItem, NewSeason};
use brarr_orchestrator::db::{grabs, library, root_folders};
use brarr_orchestrator::{AppState, db, web};

struct Harness {
    addr: SocketAddr,
    state: AppState,
    base: PathBuf,
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

async fn spawn(tag: &str) -> Harness {
    let base = std::env::temp_dir().join(format!("brarr-import-ui-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(base.join("midias")).unwrap();
    std::fs::create_dir_all(base.join("torrents")).unwrap();

    let pool = db::open_memory().await.expect("open in-memory db");
    root_folders::insert(
        &pool,
        &base.join("midias").to_string_lossy(),
        Some(MediaType::Tv),
    )
    .await
    .unwrap();
    let state = AppState::new(pool, Engine::baseline());
    let static_dir = std::env::temp_dir().join("brarr-orchestrator-import-ui-static");
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
    Harness { addr, state, base }
}

async fn add_series(state: &AppState) -> uuid::Uuid {
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
        &[NewSeason {
            season_number: 4,
            episode_count: 8,
            air_date: None,
            episodes: (1..=8)
                .map(|n| NewEpisode {
                    episode_number: n,
                    title: Some(format!("Episódio {n}")),
                    air_date: None,
                })
                .collect(),
        }],
    )
    .await
    .unwrap();
    item.id
}

/// Pull every `name="fp"` value out of the rendered dialog. This is the
/// same string the browser would post back.
fn fingerprints(html: &str) -> Vec<String> {
    html.split("name=\"fp\" value=\"")
        .skip(1)
        .filter_map(|rest| rest.split('"').next().map(str::to_owned))
        .collect()
}

fn tree(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(rel) = path.strip_prefix(dir) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    out.sort();
    out
}

#[tokio::test]
async fn the_dialog_offers_a_file_and_confirming_adopts_it_in_place() {
    let h = spawn("inplace").await;
    let item = add_series(&h.state).await;
    let folder = h.base.join("midias").join("The Boys").join("Season 4");
    std::fs::create_dir_all(&folder).unwrap();
    let file = folder.join("The.Boys.S04E07.1080p.WEB-DL-NTb.mkv");
    std::fs::write(&file, b"video").unwrap();

    let client = reqwest::Client::new();
    let body = client
        .get(format!("http://{}/library/import", h.addr))
        .query(&[(
            "folder",
            h.base.join("midias").to_string_lossy().to_string(),
        )])
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert!(body.contains("Importar do disco"), "{body}");
    assert!(body.contains("The.Boys.S04E07.1080p.WEB-DL-NTb.mkv"));
    assert!(
        body.contains("manter no lugar"),
        "a file under the root is adopted where it stands"
    );
    let fps = fingerprints(&body);
    assert_eq!(fps.len(), 1, "one row carries a target");

    let before = tree(&h.base);
    let resp = client
        .post(format!("http://{}/library/import", h.addr))
        .form(&[
            (
                "folder",
                h.base.join("midias").to_string_lossy().to_string(),
            ),
            ("action", "import".to_owned()),
            ("sel", file.to_string_lossy().to_string()),
            ("fp", fps[0].clone()),
        ])
        .send()
        .await
        .unwrap();
    let report = resp.text().await.unwrap();

    assert!(report.contains("Importação concluída"), "{report}");
    assert!(report.contains("adotado no lugar"));
    assert_eq!(
        tree(&h.base),
        before,
        "adopting in place must not create, move or copy anything"
    );

    let stored = grabs::for_item(h.state.pool(), item).await.unwrap();
    assert_eq!(stored.len(), 1);
    assert!(grabs::is_in_place(&stored[0]));
}

/// The checkbox is what selects; a row left unticked is not imported
/// even though its fingerprint was posted along with every other row's.
#[tokio::test]
async fn a_row_left_unticked_is_not_imported() {
    let h = spawn("unticked").await;
    let item = add_series(&h.state).await;
    let folder = h.base.join("midias").join("The Boys");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("The.Boys.S04E01.mkv"), b"a").unwrap();
    std::fs::write(folder.join("The.Boys.S04E02.mkv"), b"b").unwrap();

    let client = reqwest::Client::new();
    let root = h.base.join("midias").to_string_lossy().to_string();
    let body = client
        .get(format!("http://{}/library/import", h.addr))
        .query(&[("folder", root.clone())])
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let fps = fingerprints(&body);
    assert_eq!(fps.len(), 2);

    // Post both fingerprints but tick only the first episode.
    let keep = folder
        .join("The.Boys.S04E01.mkv")
        .to_string_lossy()
        .to_string();
    let report = client
        .post(format!("http://{}/library/import", h.addr))
        .form(&[
            ("folder", root.clone()),
            ("action", "import".to_owned()),
            ("sel", keep),
            ("fp", fps[0].clone()),
            ("fp", fps[1].clone()),
        ])
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert!(report.contains("Importação concluída"), "{report}");
    let stored = grabs::for_item(h.state.pool(), item).await.unwrap();
    assert_eq!(stored.len(), 1, "only the ticked row was written");
}

/// Ignoring is remembered across dialogs, and the list is the way back.
#[tokio::test]
async fn ignoring_a_file_survives_reopening_and_can_be_undone() {
    let h = spawn("ignore").await;
    add_series(&h.state).await;
    let folder = h.base.join("torrents");
    let junk = folder.join("The.Boys.S04E03.mkv");
    std::fs::write(&junk, b"x").unwrap();

    let client = reqwest::Client::new();
    let dir = folder.to_string_lossy().to_string();
    let open = format!("http://{}/library/import", h.addr);

    let body = client
        .get(&open)
        .query(&[("folder", dir.clone())])
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("The.Boys.S04E03.mkv"));

    let after_ignore = client
        .post(format!("http://{}/library/import", h.addr))
        .form(&[
            ("folder", dir.clone()),
            ("action", "ignore".to_owned()),
            ("sel", junk.to_string_lossy().to_string()),
        ])
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        !after_ignore.contains("name=\"fp\""),
        "an ignored file is no longer offered"
    );
    assert!(after_ignore.contains("Ignorados 1"));

    // Reopening from scratch still hides it — this is the difference
    // from Sonarr and Radarr, where ignoring lasts one dialog.
    let reopened = client
        .get(&open)
        .query(&[("folder", dir.clone())])
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(reopened.contains("Ignorados 1"));
    assert!(!reopened.contains("name=\"fp\""));

    let restored = client
        .post(format!("http://{}/library/import/unignore", h.addr))
        .form(&[
            ("path", junk.to_string_lossy().to_string()),
            ("folder", dir.clone()),
        ])
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(restored.contains("Ignorados 0"));
}

/// A folder brarr cannot read is a form error inside the dialog, not a
/// 500 — the operator retypes the path in the field that is already
/// there. In Docker a wrong path is the single most likely mistake.
#[tokio::test]
async fn an_unreadable_folder_is_a_form_error() {
    let h = spawn("badfolder").await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{}/library/import", h.addr))
        .query(&[("folder", "/nao/existe/em/lugar/nenhum")])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "a bad path is not a 500");
    let body = resp.text().await.unwrap();
    assert!(body.contains("Importar do disco"), "the dialog still opens");
    assert!(
        body.contains("bg-danger-soft"),
        "the reason is shown inside the dialog: {body}"
    );
    assert!(
        !body.contains("name=\"fp\""),
        "and nothing is offered for import"
    );
}
