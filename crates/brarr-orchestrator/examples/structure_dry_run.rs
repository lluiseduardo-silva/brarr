//! Run the structure dry run against a database file, and print it.
//!
//! ```text
//! cargo run -p brarr-orchestrator --example structure_dry_run -- <path-to.db>
//! ```
//!
//! ## Why this exists rather than `brarr structure --dry-run`
//!
//! The CLI subcommand talks to a running orchestrator, and **the
//! orchestrator must never be pointed at a copy of the production
//! database**: it loads the operator's real download-client and tracker
//! credentials and spawns the poller, the scanner and the importer,
//! which would then act on real infrastructure from a machine that is
//! only supposed to be reading. That rule was written after a
//! verification script wrote to production and switched off the one
//! special the operator was monitoring.
//!
//! This binary honours the reason behind the rule rather than the letter
//! of it. It opens the pool, builds the metadata registry and calls
//! [`flip::preview_all`] — which writes nothing, contacts no tracker and
//! no download client, and reaches only TMDB and `TheTVDB`, the two
//! read-only metadata APIs the report is *about*. No background task is
//! ever spawned, because none of them is started outside `main.rs`.
//!
//! Opening the file **does** run migrations on it, which is deliberate:
//! applying cleanly to a copy of the real catalogue is the other thing
//! this pass is meant to establish.

use std::collections::BTreeMap;
use std::process::ExitCode;

use brarr_orchestrator::db;
use brarr_orchestrator::flip::{self, Outcome};
use brarr_orchestrator::metadata::registry::Registry;

#[allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "a report binary; its output is the point"
)]
#[tokio::main]
async fn main() -> ExitCode {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("uso: structure_dry_run <caminho-do-banco.db>");
        return ExitCode::FAILURE;
    };

    let pool = match db::open(&path).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("não consegui abrir {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let registry = match Registry::build(&pool).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("não consegui montar o registro de provedores: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("provedores configurados: {registry:?}");

    let previews = match flip::preview_all(&pool, &registry).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("a simulação falhou: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut ready = 0_usize;
    let mut blocked = 0_usize;
    let mut refused = 0_usize;
    let mut untouched = 0_usize;

    println!("\n== PRONTOS ==");
    for p in &previews {
        match &p.outcome {
            Outcome::Untouched => untouched += 1,
            Outcome::Blocked {
                destination,
                reason,
            } => {
                blocked += 1;
                let dest = destination.as_ref().map_or_else(
                    || "—".to_owned(),
                    |d| format!("{}/{}", d.source.label(), d.ordering.family().label()),
                );
                println!("  [BLOQUEADO] {:<42} {dest:<16} {reason}", p.title);
            }
            Outcome::Ready(r) => {
                ready += 1;
                if !r.would_commit() {
                    refused += 1;
                }
                print_ready(&p.title, r);
            }
        }
    }

    println!(
        "\n== RESUMO ==\n  {} título(s) no catálogo\n  {ready} pronto(s), dos quais {refused} \
         seriam RECUSADOS pelos portões\n  {blocked} bloqueado(s)\n  {untouched} sem numeração a \
         mover\n  nada foi escrito",
        previews.len()
    );
    ExitCode::SUCCESS
}

/// One title that can move, and the lines that explain how.
#[allow(
    clippy::print_stdout,
    reason = "a report binary; its output is the point"
)]
fn print_ready(title: &str, r: &flip::Ready) {
    let handle = r.destination.ordering.handle().unwrap_or("");
    let (stored, incoming) = r.plan.air_date_coverage;
    println!(
        "  {:<42} {}/{}{}{}  pareados {:>4} · órfãos {:>3} · novos {:>3} · \
         datas {:>3.0}%/{:>3.0}%{}{}",
        title,
        r.destination.source.label(),
        r.destination.ordering.family().label(),
        if handle.is_empty() {
            String::new()
        } else {
            format!(":{handle}")
        },
        if r.destination.pinned { " [fix]" } else { "" },
        r.plan.pairs.len(),
        r.plan.orphans.len(),
        r.plan.added,
        stored * 100.0,
        incoming * 100.0,
        if r.plan.grabs_at_risk() > 0 {
            format!("  ⚠ {} aquisição(ões) em risco", r.plan.grabs_at_risk())
        } else {
            String::new()
        },
        r.refusal()
            .map(|w| format!("\n      ↳ RECUSADO: {w}"))
            .unwrap_or_default(),
    );

    for pack in &r.plan.packs_affected {
        println!(
            "      ↳ pacote da temporada {}: {} → {} episódio(s) ({} aquisição/ões)",
            pack.season, pack.was, pack.now, pack.grabs
        );
    }

    // Where the orphans sit. A refusal is a number until you know which
    // season it is about, and season 0 being the whole of it is a very
    // different problem from season 4 being part of it: the first is two
    // catalogues disagreeing about how many extras a series has, the
    // second is an episode with a file that one of them does not list.
    if !r.plan.orphans.is_empty() {
        let mut per: BTreeMap<i32, usize> = BTreeMap::new();
        for o in &r.plan.orphans {
            *per.entry(o.season).or_default() += 1;
        }
        let breakdown: Vec<String> = per
            .iter()
            .map(|(season, n)| format!("T{season}×{n}"))
            .collect();
        println!("      ↳ órfãos por temporada: {}", breakdown.join(" "));
    }
}
