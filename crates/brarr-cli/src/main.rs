//! Entry point do binário `brarr`. Casca fina sobre a [`brarr_cli`] lib:
//! parseia args, inicializa logging, despacha o subcomando.
//!
//! Erro propagation usa `anyhow` (convenção para binários — agrega
//! qualquer tipo de erro `: Error` com contexto encadeado e imprime
//! cadeia completa via `{:#}`).

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use brarr_cli::{
    Cli, Command, Config, Engine, MaintenanceArgs, OutputFormat, RemoteArgs, RuleSet, SearchArgs,
    StructureArgs, format_structure, format_structure_json, run_remote_maintenance,
    run_remote_search, run_remote_structure, run_search,
};
use brarr_core::TmdbId;
use clap::Parser;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

fn main() -> ExitCode {
    let cli = Cli::parse();
    if let Err(e) = init_tracing(&cli) {
        let mut stderr = std::io::stderr().lock();
        let _ = writeln!(stderr, "Falha ao inicializar logging: {e:#}");
        return ExitCode::from(2);
    }

    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let mut stderr = std::io::stderr().lock();
            let _ = writeln!(stderr, "Erro: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn init_tracing(cli: &Cli) -> Result<()> {
    let directive = cli.log_directive();
    let filter = EnvFilter::try_new(directive)
        .with_context(|| format!("invalid tracing filter {directive:?}"))?;
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .try_init()
        .context("could not install tracing subscriber")?;
    Ok(())
}

fn run(cli: &Cli) -> Result<()> {
    match &cli.command {
        Command::Search(args) => {
            let config_path = resolve_config_path(cli.config.clone())?;
            let config = Config::load(&config_path)
                .with_context(|| format!("loading config from {}", config_path.display()))?;
            dispatch_search(&config, args)
        }
        Command::Remote(args) => dispatch_remote(args),
        Command::Maintenance(args) => dispatch_maintenance(args),
        Command::Structure(args) => dispatch_structure(args),
    }
}

fn resolve_config_path(explicit: Option<PathBuf>) -> Result<PathBuf> {
    match explicit {
        Some(p) => Ok(p),
        None => Config::default_path()
            .context("resolving default config path; pass --config explicitly to override"),
    }
}

fn dispatch_search(config: &Config, args: &SearchArgs) -> Result<()> {
    let tmdb = TmdbId::new(args.tmdb)
        .with_context(|| format!("TMDB id {} is invalid (must be > 0)", args.tmdb))?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;

    let engine = if config.rules.is_empty() {
        Engine::baseline()
    } else {
        Engine::new(RuleSet {
            rules: config.rules.clone(),
        })
    };

    let outcome = runtime
        .block_on(run_search(&config.trackers, tmdb, &engine))
        .context("running search across trackers")?;

    let rendered = match args.format {
        OutputFormat::Text => brarr_cli::format_outcome(&outcome, args.limit),
        OutputFormat::Json => brarr_cli::format_outcome_json(&outcome, args.limit)
            .context("serializing search outcome to JSON")?,
    };
    // Stdout é a saída user-facing oficial da CLI. O lint `print_stdout`
    // existe pra impedir uso acidental em código de lib/serviço — aqui
    // é deliberado.
    #[allow(
        clippy::print_stdout,
        reason = "CLI user-facing output goes to stdout by design"
    )]
    {
        match args.format {
            OutputFormat::Text => print!("{rendered}"),
            OutputFormat::Json => println!("{rendered}"),
        }
    }

    if outcome.scored.is_empty() && !outcome.failures.is_empty() {
        anyhow::bail!(
            "nenhum release retornado e {} tracker(s) falharam — veja os erros acima",
            outcome.failures.len(),
        );
    }

    Ok(())
}

fn dispatch_remote(args: &RemoteArgs) -> Result<()> {
    let tmdb = match args.tmdb {
        Some(n) => {
            Some(TmdbId::new(n).with_context(|| format!("TMDB id {n} is invalid (must be > 0)"))?)
        }
        None => None,
    };
    let imdb = match args.imdb.as_deref() {
        Some(raw) => {
            let stripped = raw.trim().trim_start_matches("tt");
            let n: u32 = stripped
                .parse()
                .with_context(|| format!("invalid IMDB id {raw:?}: expected numeric tt-id"))?;
            Some(
                brarr_core::ImdbId::new(n)
                    .with_context(|| format!("IMDB id {raw} is invalid (must be > 0)"))?,
            )
        }
        None => None,
    };
    if tmdb.is_none() && imdb.is_none() {
        anyhow::bail!("at least one of --tmdb or --imdb must be set");
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;
    let outcome = runtime
        .block_on(run_remote_search(
            &args.addr,
            args.token.as_deref(),
            tmdb,
            imdb,
        ))
        .with_context(|| format!("remote search against {}", args.addr))?;

    let rendered = match args.format {
        OutputFormat::Text => brarr_cli::format_outcome(&outcome, args.limit),
        OutputFormat::Json => brarr_cli::format_outcome_json(&outcome, args.limit)
            .context("serializing search outcome to JSON")?,
    };
    #[allow(
        clippy::print_stdout,
        reason = "CLI user-facing output goes to stdout by design"
    )]
    {
        match args.format {
            OutputFormat::Text => print!("{rendered}"),
            OutputFormat::Json => println!("{rendered}"),
        }
    }
    Ok(())
}

fn dispatch_maintenance(args: &MaintenanceArgs) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;
    let out = runtime
        .block_on(run_remote_maintenance(
            &args.addr,
            args.token.as_deref(),
            args.vacuum,
        ))
        .with_context(|| format!("remote maintenance against {}", args.addr))?;

    #[allow(
        clippy::print_stdout,
        reason = "CLI user-facing output goes to stdout by design"
    )]
    {
        println!(
            "Manutenção concluída: {} decisão(ões) e {} busca(s) removidas (janela de {} dia(s)){}.",
            out.decisions_deleted,
            out.searches_deleted,
            out.retention_days,
            if args.vacuum { " + VACUUM" } else { "" }
        );
    }
    Ok(())
}

/// `brarr structure --dry-run`: ask the orchestrator what changing each
/// title's structure owner would do, and print it.
///
/// The `--dry-run` flag is required and is currently the only mode. A
/// required flag with one legal value looks like ceremony; here it is
/// the opposite. This command will grow an apply mode, and the
/// difference between "tell me" and "rewrite my library" is exactly the
/// kind that belongs written on the command line rather than inherited
/// from a default — so no invocation anyone has already typed changes
/// meaning the day `--apply` exists.
fn dispatch_structure(args: &StructureArgs) -> Result<()> {
    if !args.dry_run {
        anyhow::bail!(
            "passe --dry-run: por enquanto este comando só relata, e a flag existe \
             para que a versão que escreve nunca seja o default"
        );
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;
    let titles = runtime
        .block_on(run_remote_structure(
            &args.addr,
            args.token.as_deref(),
            args.item.as_deref(),
        ))
        .with_context(|| format!("remote structure dry run against {}", args.addr))?;

    let rendered = match args.format {
        OutputFormat::Text => format_structure(&titles),
        OutputFormat::Json => {
            format_structure_json(&titles).context("serialising the structure report")?
        }
    };

    #[allow(
        clippy::print_stdout,
        reason = "CLI user-facing output goes to stdout by design"
    )]
    {
        println!("{rendered}");
    }
    Ok(())
}
