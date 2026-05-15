//! Definição dos argumentos de linha de comando via `clap` derive.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

/// Brarr — agregador de busca em trackers `UNIT3D` com foco em PT-BR.
#[derive(Debug, Parser)]
#[command(version, about, long_about = None)]
pub struct Cli {
    /// Caminho para o arquivo de configuração TOML.
    ///
    /// Default: `$XDG_CONFIG_HOME/brarr/config.toml` (Linux/macOS) ou
    /// `%APPDATA%\brarr\config.toml` (Windows).
    #[arg(short, long, global = true)]
    pub config: Option<PathBuf>,

    /// Aumenta a verbosidade do log. Use `-v` para info, `-vv` para
    /// debug, `-vvv` para trace. Sem flag = `warn`.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Comando a executar.
    #[command(subcommand)]
    pub command: Command,
}

/// Subcomandos disponíveis.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Busca releases em todos os trackers configurados, em paralelo.
    Search(SearchArgs),
}

/// Argumentos do subcomando `search`.
#[derive(Debug, Args)]
pub struct SearchArgs {
    /// ID `TMDB` do filme/série (e.g., `603` para The Matrix 1999).
    #[arg(long)]
    pub tmdb: u32,

    /// Quantos releases mostrar, ordenados por score (decrescente).
    #[arg(long, default_value_t = 10)]
    pub limit: usize,

    /// Formato de saída.
    ///
    /// - `text` (default): tabela humana com flags PT/HDR.
    /// - `json`: objeto JSON em uma só linha, útil para pipe em `jq` ou
    ///   integração com outras ferramentas.
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub format: OutputFormat,
}

/// Formato escolhido para `brarr search`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    /// Saída legível em texto plano.
    Text,
    /// Saída JSON, uma linha, pronta para pipe.
    Json,
}

impl Cli {
    /// Converte o contador de `-v` em uma diretiva para `tracing-subscriber`.
    #[must_use]
    pub fn log_directive(&self) -> &'static str {
        match self.verbose {
            0 => "brarr_cli=warn,brarr_tracker_unit3d=warn",
            1 => "brarr_cli=info,brarr_tracker_unit3d=info",
            2 => "brarr_cli=debug,brarr_tracker_unit3d=debug,brarr_mediainfo=debug",
            _ => "trace",
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::{Cli, Command};
    use clap::Parser;

    #[test]
    fn parses_search_with_tmdb_only() {
        let cli = Cli::try_parse_from(["brarr", "search", "--tmdb", "603"]).expect("valid args");
        match cli.command {
            Command::Search(args) => {
                assert_eq!(args.tmdb, 603);
                assert_eq!(args.limit, 10);
                assert_eq!(args.format, super::OutputFormat::Text);
            }
        }
        assert_eq!(cli.verbose, 0);
        assert!(cli.config.is_none());
    }

    #[test]
    fn parses_search_with_json_format() {
        let cli = Cli::try_parse_from(["brarr", "search", "--tmdb", "603", "--format", "json"])
            .expect("valid args");
        let Command::Search(args) = cli.command;
        assert_eq!(args.format, super::OutputFormat::Json);
    }

    #[test]
    fn parses_search_with_explicit_limit_and_config() {
        let cli = Cli::try_parse_from([
            "brarr",
            "--config",
            "/tmp/cfg.toml",
            "search",
            "--tmdb",
            "603",
            "--limit",
            "5",
        ])
        .expect("valid args");
        let Command::Search(args) = cli.command;
        assert_eq!(args.tmdb, 603);
        assert_eq!(args.limit, 5);
        assert_eq!(
            cli.config.as_deref(),
            Some(std::path::Path::new("/tmp/cfg.toml"))
        );
    }

    #[test]
    fn verbose_flag_counts() {
        let cli = Cli::try_parse_from(["brarr", "-vvv", "search", "--tmdb", "1"]).expect("valid");
        assert_eq!(cli.verbose, 3);
        assert_eq!(cli.log_directive(), "trace");
    }

    #[test]
    fn log_directive_progressively_verbose() {
        let make = |v: u8| Cli {
            config: None,
            verbose: v,
            command: Command::Search(super::SearchArgs {
                tmdb: 1,
                limit: 1,
                format: super::OutputFormat::Text,
            }),
        };
        assert!(make(0).log_directive().contains("warn"));
        assert!(make(1).log_directive().contains("info"));
        assert!(make(2).log_directive().contains("debug"));
        assert_eq!(make(3).log_directive(), "trace");
    }

    #[test]
    fn rejects_unknown_subcommand() {
        let r = Cli::try_parse_from(["brarr", "nope"]);
        assert!(r.is_err());
    }

    #[test]
    fn requires_tmdb_for_search() {
        let r = Cli::try_parse_from(["brarr", "search"]);
        assert!(r.is_err());
    }
}
