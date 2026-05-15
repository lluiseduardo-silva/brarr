//! `brarr-mediainfo` — parser de dumps textuais do `MediaInfo`.
//!
//! Transforma a saída textual bruta do `mediainfo` (idêntica ao campo
//! `mediainfo` retornado por trackers `UNIT3D`) em estruturas tipadas
//! ([`ParsedMediaInfo`], [`AudioTrack`], [`SubtitleTrack`], [`VideoTrack`],
//! [`GeneralInfo`]) usando o enum [`Language`] de `brarr-core` para
//! normalizar idiomas.
//!
//! # Exemplo
//!
//! ```
//! use brarr_mediainfo::{parse, Language};
//!
//! # fn main() -> Result<(), brarr_mediainfo::ParseError> {
//! let dump = "\
//! General
//! Format                                   : Matroska
//!
//! Audio
//! Format                                   : E-AC-3
//! Channel(s)                               : 2 channels
//! Language                                 : Portuguese
//! Title                                    : Brazilian Portuguese
//! Default                                  : Yes
//! Forced                                   : No
//! ";
//! let info = parse(dump)?;
//! assert_eq!(info.audio.len(), 1);
//! assert_eq!(info.audio[0].language, Language::PtBr);
//! assert_eq!(info.audio[0].channels, Some(2));
//!
//! // Conversão para o "destilado" consumido pelos scorers:
//! let enrichment = info.to_enrichment();
//! assert!(enrichment.has_audio_in(&Language::PtBr));
//! # Ok(())
//! # }
//! ```
//!
//! O parser é **tolerante a campos desconhecidos**: chaves não mapeadas
//! são silenciosamente ignoradas e valores numéricos inválidos viram
//! `None`. [`ParseError`] é reservado a problemas estruturais
//! (entrada vazia, nenhuma seção reconhecida).
//!
//! Quebras de linha `\r\n` (saída de Windows/UNIT3D) e `\n` (Unix)
//! ambas funcionam — a tokenização normaliza antes de processar.

// Padrão comum em libs Rust: tipos têm o nome do módulo embutido para
// uso ergonômico após `pub use`. Não vale gritar `clippy::module_name_repetitions`
// para `ParseError` em `mod error`, etc.
#![allow(clippy::module_name_repetitions)]

mod enrichment;
mod error;
mod parser;
mod types;

pub use error::ParseError;
pub use parser::parse;
pub use types::{AudioTrack, GeneralInfo, ParsedMediaInfo, SubtitleTrack, VideoTrack};

// Re-export para que consumidores de `brarr-mediainfo` não precisem
// adicionar `brarr-core` como dep só pra mencionar o enum `Language`.
pub use brarr_core::Language;
