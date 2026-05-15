//! `brarr-core` — tipos de domínio compartilhados entre crates.
//!
//! Define o vocabulário que `brarr-tracker-unit3d`, `brarr-cli`,
//! `brarr-orchestrator` e `brarr-decision-service` falam entre si:
//! [`Release`], [`TrackerSource`], [`Language`], [`Resolution`],
//! [`ReleaseKind`], newtypes para IDs externos ([`TmdbId`], [`ImdbId`],
//! [`TvdbId`], [`MalId`]), [`DecisionScore`] com invariante de faixa,
//! e [`ReleaseEnrichment`] (representação simplificada de
//! `ParsedMediaInfo` pra consumo dos scorers).
//!
//! Princípios:
//! - **Camada folha**: sem `HTTP`, sem async, sem regras, sem
//!   dependência em `brarr-mediainfo` ou outros crates de aplicação.
//! - **Tipos como documentação**: enums no lugar de strings mágicas,
//!   newtypes no lugar de inteiros nus, construtores que validam
//!   invariantes (e devolvem `Result`).
//! - **`#[non_exhaustive]`** em structs públicos para permitir
//!   adicionar campos sem quebrar consumidores externos.

#![allow(clippy::module_name_repetitions)]

mod enrichment;
mod ids;
mod language;
mod release;
mod score;
mod tracker;

pub use enrichment::ReleaseEnrichment;
pub use ids::{ImdbId, ImdbIdError, MalId, MalIdError, TmdbId, TmdbIdError, TvdbId, TvdbIdError};
pub use language::Language;
pub use release::{ExternalIds, Release, ReleaseError, ReleaseKind, ReleaseUrls, Resolution};
pub use score::{DecisionScore, ScoreOutOfRange};
pub use tracker::{TrackerSource, TrackerSourceError};
