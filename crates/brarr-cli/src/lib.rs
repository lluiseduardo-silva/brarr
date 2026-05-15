//! `brarr-cli` library portion — exposta para que testes de integração
//! exercitem o pipeline de busca (parse de config → fan-out em
//! `brarr-tracker-unit3d` clients → scoring → format) sem invocar o
//! binário.
//!
//! O binário (`src/main.rs`) é uma casca fina que parseia CLI args,
//! inicializa o subscriber de `tracing`, e delega o trabalho real para
//! [`run`].
//!
//! Erros de domínio (config, scoring) usam `thiserror`. O binário
//! agrega tudo com `anyhow` no `main()`.

#![allow(clippy::module_name_repetitions)]

pub mod cli;
pub mod config;
pub mod search;

pub use cli::{Cli, Command, OutputFormat, SearchArgs};
pub use config::{Config, ConfigError, TrackerConfig};
pub use search::{ScoredRelease, SearchOutcome, format_outcome, format_outcome_json, run_search};

// Re-export do motor de decisão para que callers (tests, main.rs) não
// precisem importar `brarr_decision_service` separadamente.
pub use brarr_decision_service::{Engine, RuleSet};
