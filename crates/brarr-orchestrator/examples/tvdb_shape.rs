//! What shape `TheTVDB` reports for one title, under every season type.
//!
//! ```text
//! cargo run -p brarr-orchestrator --example tvdb_shape -- <db> "<título>"
//! ```
//!
//! A diagnostic, not a feature. The structure dry run refused a third of
//! the batch on orphaned specials, and the fix depends on something only
//! the provider can answer: whether `TheTVDB` models season 0 for these
//! series at all, or merely omits it from the ordering brarr asks for.
//! Read-only, one title, no writes anywhere.

use std::process::ExitCode;

use brarr_core::{MetadataSource, Ordering, OrderingFamily};
use brarr_orchestrator::db::{self, item_ids, library};
use brarr_orchestrator::metadata::registry::Registry;

#[allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "a diagnostic binary; its output is the point"
)]
#[tokio::main]
async fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let (Some(path), Some(needle)) = (args.next(), args.next()) else {
        eprintln!("uso: tvdb_shape <banco.db> <trecho-do-título>");
        return ExitCode::FAILURE;
    };

    let Ok(pool) = db::open(&path).await else {
        eprintln!("não consegui abrir {path}");
        return ExitCode::FAILURE;
    };
    let Ok(items) = library::list(&pool).await else {
        eprintln!("não consegui ler o catálogo");
        return ExitCode::FAILURE;
    };
    let Some(item) = items
        .iter()
        .find(|i| i.title.to_lowercase().contains(&needle.to_lowercase()))
    else {
        eprintln!("nenhum título contém {needle:?}");
        return ExitCode::FAILURE;
    };
    println!("== {} ==", item.title);

    // What brarr holds today.
    let stored = library::episodes(&pool, item.id).await.unwrap_or_default();
    let mut per: std::collections::BTreeMap<i32, usize> = std::collections::BTreeMap::new();
    for e in &stored {
        *per.entry(e.season_number).or_default() += 1;
    }
    println!(
        "armazenado (TMDB): {}",
        per.iter()
            .map(|(s, n)| format!("T{s}×{n}"))
            .collect::<Vec<_>>()
            .join(" ")
    );

    let Ok(registry) = Registry::build(&pool).await else {
        eprintln!("não consegui montar o registro");
        return ExitCode::FAILURE;
    };
    let Ok(ids) = item_ids::for_item(&pool, item.id).await else {
        eprintln!("não consegui ler os ids");
        return ExitCode::FAILURE;
    };
    let Some(known) = ids.iter().find(|s| s.id.source() == MetadataSource::Tvdb) else {
        eprintln!("o título não guarda um id da TheTVDB");
        return ExitCode::FAILURE;
    };
    let Ok(tvdb) = registry.require(MetadataSource::Tvdb) else {
        eprintln!("a TheTVDB não está configurada neste banco");
        return ExitCode::FAILURE;
    };

    // Every ordering the adapter can ask for, so "does season 0 exist
    // anywhere" is answered rather than inferred from one call.
    let orderings: Vec<(&str, Ordering)> = vec![
        ("default (official)", Ordering::Default),
        ("dvd", named(OrderingFamily::Dvd, "dvd")),
        ("absolute", named(OrderingFamily::Absolute, "absolute")),
        ("alternate", named(OrderingFamily::Alternate, "alternate")),
        ("regional", named(OrderingFamily::Other, "regional")),
        (
            "o tipo default da série",
            named(OrderingFamily::Other, "default"),
        ),
    ];

    for (label, ordering) in orderings {
        match tvdb.tree(&known.id, &ordering).await {
            Ok(tree) => {
                let shape: Vec<String> = tree
                    .seasons
                    .iter()
                    .map(|s| format!("T{}×{}", s.number, s.episodes.len()))
                    .collect();
                println!("{label:<24} {}", shape.join(" "));
            }
            Err(e) => println!("{label:<24} — {e}"),
        }
    }

    ExitCode::SUCCESS
}

fn named(family: OrderingFamily, handle: &str) -> Ordering {
    Ordering::Named {
        family,
        handle: handle.into(),
    }
}
