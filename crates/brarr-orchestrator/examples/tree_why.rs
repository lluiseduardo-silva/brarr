//! Why one title's tree write is being refused.
//!
//! ```text
//! cargo run -p brarr-orchestrator --example tree_why -- <db> "<título>"
//! ```
//!
//! The passive \*arr sweep collects a per-title failure reason into
//! `ImportReport::failures` and **logs only the count**, so an operator
//! sees `failed=2` with no way to learn why. This runs the same two calls
//! `arr_import::sync_tree` makes — `metadata::owned::tree` then
//! `structure::plan` — and prints what they say.
//!
//! **It writes to the database you give it — point it at a copy.** The
//! read-only half was tried first and was not enough: `plan` reported
//! "the gate would accept" for both failing titles, because the defect
//! was a `UNIQUE` violation inside `write_tree`, past every gate. A
//! diagnostic that stops before the write cannot see what the write
//! sees.

use std::process::ExitCode;

use brarr_orchestrator::db::{self, library};
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
    let (Some(path), Some(needle)) = (args.next(), args.next()) else {
        eprintln!("uso: tree_why <banco.db> <trecho-do-título>");
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
        .into_iter()
        .find(|i| i.title.to_lowercase().contains(&needle.to_lowercase()))
    else {
        eprintln!("nenhum título casa com {needle}");
        return ExitCode::FAILURE;
    };
    println!("== {} ==", item.title);

    let registry = match Registry::build(&pool).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("registro: {e}");
            return ExitCode::FAILURE;
        }
    };

    let tree = match brarr_orchestrator::metadata::owned::tree(&pool, &registry, item.id).await {
        Ok(t) => t,
        Err(e) => {
            println!("owned::tree RECUSOU: {e}");
            return ExitCode::SUCCESS;
        }
    };
    println!(
        "árvore de {}: {} temporada(s), {} episódio(s)",
        tree.source.display_name(),
        tree.seasons.len(),
        tree.episode_count()
    );

    match structure::plan(&pool, item.id, &tree).await {
        Ok(plan) => {
            println!(
                "plano: pares={} órfãos={} novos={} cobertura=({:.0}%,{:.0}%)",
                plan.pairs.len(),
                plan.orphans.len(),
                plan.added,
                plan.air_date_coverage.0 * 100.0,
                plan.air_date_coverage.1 * 100.0
            );
            match structure::refusal(&plan) {
                Some(why) => println!("portão RECUSARIA: {why}"),
                None => match structure::apply(&pool, item.id, &tree).await {
                    Ok(d) => println!(
                        "apply OK: reusados={} novos={} duplicados={}",
                        d.reused, d.added, d.duplicates
                    ),
                    Err(e) => println!("apply RECUSOU: {e}"),
                },
            }
        }
        Err(e) => println!("structure::plan RECUSOU: {e}"),
    }
    ExitCode::SUCCESS
}
