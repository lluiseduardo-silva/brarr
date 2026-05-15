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
use brarr_cli::{Cli, Command, Config, OutputFormat, ScoringWeights, SearchArgs, run_search};
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
    let config_path = resolve_config_path(cli.config.clone())?;
    let config = Config::load(&config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;

    match &cli.command {
        Command::Search(args) => dispatch_search(&config, args),
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

    let outcome = runtime
        .block_on(run_search(
            &config.trackers,
            tmdb,
            &ScoringWeights::default(),
        ))
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
