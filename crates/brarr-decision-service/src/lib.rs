//! `brarr-decision-service` — motor de regras declarativas para
//! ranqueamento e filtragem de releases.
//!
//! Recebe um [`brarr_core::Release`] (já enriquecido pelo parser de
//! `MediaInfo`) e aplica um conjunto de regras configurado em TOML,
//! produzindo um [`DecisionOutcome`] com score, tags coletadas, flag
//! de rejeição, e lista das regras que casaram.
//!
//! ## Filosofia
//!
//! Substitui o `score_release` hardcoded da Fase 5 — `brarr-cli` ainda
//! aceita o motor pra manter compatibilidade, mas a lógica de pontuação
//! agora é **declarativa** e configurável por usuário sem recompilar.
//!
//! Não fala HTTP, não conhece UNIT3D, não interage com tracker — depende
//! só de `brarr-core`.
//!
//! ## Schema TOML
//!
//! ```toml
//! [[rule]]
//! name = "PT-BR audio"
//! add_score = 100
//! when = { audio = "pt-br" }
//!
//! [[rule]]
//! name = "Reject < 720p"
//! reject = true
//! when = {}  # always match — use junto com condition negativa em outras regras
//! ```
//!
//! ## Exemplo
//!
//! ```no_run
//! use brarr_decision_service::{Engine, RuleSet, loader};
//!
//! let toml_str = r#"
//! [[rule]]
//! name = "PT-BR"
//! add_score = 100
//! when = { audio = "pt-br" }
//! "#;
//! let rules: RuleSet = loader::from_toml(toml_str).expect("parse");
//! let engine = Engine::new(rules);
//!
//! // Em código real: outcome = engine.evaluate(&release);
//! # let _ = engine;
//! ```

#![allow(clippy::module_name_repetitions)]

mod engine;
pub mod loader;
mod outcome;
mod rule;

pub use engine::Engine;
pub use loader::{LoadError, from_toml};
pub use outcome::DecisionOutcome;
pub use rule::{
    AudioFilter, CodecFilter, Condition, KindFilter, ResolutionFilter, Rule, RuleSet,
    SubtitleFilter,
};
