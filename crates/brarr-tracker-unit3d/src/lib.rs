//! `brarr-tracker-unit3d` — cliente HTTP async para trackers baseados em `UNIT3D`.
//!
//! Cobre os endpoints relevantes para busca:
//! - `GET /api/torrents/filter?tmdbId=<id>` — lista de torrents
//! - `GET /api/torrents/{id}` — torrent específico
//!
//! Desserializa o `JSON` em DTOs internos e converte para
//! [`brarr_core::Release`], populando o campo `enrichment` via
//! [`brarr_mediainfo::parse`] quando o tracker fornece o dump de
//! `MediaInfo`. **Library pura** — quem orquestra retries, paralelismo
//! entre trackers ou cache é o `brarr-cli` / `brarr-orchestrator`.
//!
//! ## Como conseguir um token `UNIT3D`
//!
//! 1. Logar na conta no tracker (e.g., capybarabr.com, locadora.cc).
//! 2. Settings → API → "Generate New Token".
//! 3. Copiar e guardar como segredo. Tokens UNIT3D dão acesso de
//!    leitura *e* download por padrão — tratar como senha.
//!
//! ## Configuração
//!
//! [`Unit3dClient::new`] aceita um [`brarr_core::TrackerSource`]
//! (nome de display + `base_url` da API) e o token. Cria um
//! `reqwest::Client` com `Authorization: Bearer <token>` em todas as
//! requests, timeout default de 30s, e TLS nativa do sistema.
//!
//! ## Exemplo
//!
//! ```no_run
//! use brarr_core::{TmdbId, TrackerSource};
//! use brarr_tracker_unit3d::Unit3dClient;
//! use url::Url;
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let tracker = TrackerSource::new(
//!     "capybara",
//!     Url::parse("https://capybarabr.com/")?,
//! )?;
//! let client = Unit3dClient::new(tracker, "redacted-token")?;
//!
//! let releases = client.search_by_tmdb(TmdbId::new(603)?).await?;
//! for r in releases {
//!     println!("{} | {} seeders | enrichment? {}", r.title, r.seeders, r.enrichment.is_some());
//! }
//! # Ok(()) }
//! ```

#![allow(clippy::module_name_repetitions)]

mod client;
mod convert;
mod dto;
mod error;
mod provider_impl;
mod retry;

pub use client::{PingReport, Unit3dClient};
pub use convert::ConversionError;
pub use error::ClientError;
pub use retry::RetryConfig;
