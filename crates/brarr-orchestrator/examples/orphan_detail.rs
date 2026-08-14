//! Name the episodes a structure change would leave behind.
//!
//! ```text
//! cargo run -p brarr-orchestrator --example orphan_detail -- <db> [trecho-do-título]
//! ```
//!
//! The dry run counts orphans; this says which ones, and puts the
//! incoming season beside them so the disagreement is legible. Specials
//! are skipped by default — season 0 being 138 rows against 36 is two
//! catalogues disagreeing about how many extras exist, and reading it
//! line by line teaches nothing. What is worth reading one by one is an
//! episode in a **real** season that one source lists and the other does
//! not, because that is an episode with a file.
//!
//! Read-only. Nothing is written, and the destination's tree is fetched
//! exactly as the dry run fetches it.

use std::collections::BTreeMap;
use std::process::ExitCode;

use brarr_core::SeriesTree;
use brarr_orchestrator::db::{self, item_ids, library};
use brarr_orchestrator::flip::{self, Outcome};
use brarr_orchestrator::metadata::registry::Registry;
use brarr_orchestrator::structure;

#[allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "a diagnostic binary; its output is the point"
)]
#[tokio::main]
async fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("uso: orphan_detail <banco.db> [trecho-do-título]");
        return ExitCode::FAILURE;
    };
    let needle = args.next().unwrap_or_default().to_lowercase();

    let Ok(pool) = db::open(&path).await else {
        eprintln!("não consegui abrir {path}");
        return ExitCode::FAILURE;
    };
    let Ok(registry) = Registry::build(&pool).await else {
        eprintln!("não consegui montar o registro");
        return ExitCode::FAILURE;
    };
    let Ok(items) = library::list(&pool).await else {
        eprintln!("não consegui ler o catálogo");
        return ExitCode::FAILURE;
    };

    let mut titles = 0_usize;
    let mut episodes = 0_usize;
    let mut with_file = 0_i64;

    for item in &items {
        if !needle.is_empty() && !item.title.to_lowercase().contains(&needle) {
            continue;
        }
        let Ok(preview) = flip::preview(&pool, &registry, item).await else {
            continue;
        };
        let Outcome::Ready(ready) = &preview.outcome else {
            continue;
        };

        // Season 0 is excluded on purpose: see the module docs.
        let real: Vec<_> = ready.plan.orphans.iter().filter(|o| o.season > 0).collect();
        if real.is_empty() {
            continue;
        }
        titles += 1;

        // The tree the destination would write, fetched the same way the
        // dry run fetches it, so the two sides are comparable.
        let Some(incoming) = destination_tree(&pool, &registry, item, ready).await else {
            println!(
                "== {} == (não consegui buscar a árvore de destino)",
                item.title
            );
            continue;
        };

        println!("\n== {} ==", item.title);
        print_methods(ready);
        let stored = library::episodes(&pool, item.id).await.unwrap_or_default();

        let mut seasons: Vec<i32> = real.iter().map(|o| o.season).collect();
        seasons.sort_unstable();
        seasons.dedup();

        for season in seasons {
            let here: Vec<_> = real.iter().filter(|o| o.season == season).collect();
            episodes += here.len();
            with_file += here.iter().map(|o| o.grabs).sum::<i64>();
            print_season(season, &here, &stored, &incoming);
        }
    }

    println!(
        "\n== RESUMO ==\n  {titles} título(s) com órfão em temporada real\n  {episodes} \
         episódio(s), carregando {with_file} arquivo(s)\n  especiais omitidos de propósito\n  \
         nada foi escrito"
    );
    ExitCode::SUCCESS
}

/// The tree the destination would produce. Mirrors `flip::preview`.
async fn destination_tree(
    pool: &db::Pool,
    registry: &Registry,
    item: &library::LibraryItem,
    ready: &flip::Ready,
) -> Option<SeriesTree> {
    let ids = item_ids::for_item(pool, item.id).await.ok()?;
    let known = ids
        .iter()
        .find(|s| s.id.source() == ready.destination.source)?;
    let provider = registry.require(ready.destination.source).ok()?;
    provider
        .tree(&known.id, &ready.destination.ordering)
        .await
        .ok()
}

/// Which tier carried the pairing, and how much of it moved.
///
/// **This is the line that turned this diagnostic into a finding.**
/// Equal counts on both sides with one row left over is not two
/// catalogues disagreeing about content — it is a shift, and the tier
/// that produced it is the first thing worth knowing.
#[allow(
    clippy::print_stdout,
    reason = "a diagnostic binary; its output is the point"
)]
fn print_methods(ready: &flip::Ready) {
    let mut per: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for pairing in &ready.plan.pairs {
        let slot = per.entry(format!("{:?}", pairing.method)).or_insert((0, 0));
        slot.0 += 1;
        if pairing.moved {
            slot.1 += 1;
        }
    }
    let summary: Vec<String> = per
        .iter()
        .map(|(m, (n, moved))| format!("{m} {n} ({moved} movidos)"))
        .collect();
    println!("  pareamento: {}", summary.join(" · "));
}

/// One season's disagreement, both sides side by side.
#[allow(
    clippy::print_stdout,
    reason = "a diagnostic binary; its output is the point"
)]
fn print_season(
    season: i32,
    orphans: &[&&structure::Orphan],
    stored: &[library::Episode],
    incoming: &SeriesTree,
) {
    let stored_here = stored.iter().filter(|e| e.season_number == season).count();
    let found = incoming.seasons.iter().find(|s| s.number == season);
    let incoming_here = found.map_or(0, |s| s.episodes.len());

    println!("  temporada {season}: TMDB tem {stored_here}, o destino tem {incoming_here}");
    for o in orphans {
        println!(
            "    ficaria de fora  S{:02}E{:02}  {}{}",
            o.season,
            o.number,
            o.title.as_deref().unwrap_or("(sem título)"),
            if o.grabs > 0 {
                format!("   ⚠ {} arquivo(s)", o.grabs)
            } else {
                String::new()
            }
        );
    }
    if let Some(s) = found {
        println!("    o destino lista:");
        for e in &s.episodes {
            println!(
                "      S{:02}E{:02}  {}",
                season,
                e.number,
                e.title.as_deref().unwrap_or("(sem título)")
            );
        }
    }
}
